use flate2::write::GzEncoder;
use flate2::Compression;
use google_cloud_metadata::on_gce;
use google_cloud_token::TokenSourceProvider;
use google_cloudprofiler2::api::CreateProfileRequest;
use google_cloudprofiler2::api::Deployment;
use google_cloudprofiler2::api::Profile;
use google_cloudprofiler2::hyper::client::HttpConnector;
use google_cloudprofiler2::hyper_rustls::HttpsConnector;
use google_cloudprofiler2::{hyper, hyper_rustls, CloudProfiler};
use pprof::protos::Message;
use pprof::Report;
use std::collections::HashMap;
use std::default::Default;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

const SCOPES: [&str; 3] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/monitoring",
    "https://www.googleapis.com/auth/monitoring.write",
];

#[derive(Error, Debug)]
enum GcpCloudProfilingError {
    #[error("Failed to get auth token from gcp metadata server")]
    FailedToGetAuthToken(String),
    #[error("Failed to create new profile on gcp profiler server")]
    FailedToCreateProfile(String),
    #[error("Failed to profile current application")]
    FailedToProfileApplication(String),
    #[error("Failed to build pprof data from profile")]
    FailedToBuildReport(String),
    #[error("Failed to serialize profile data for transmitting to GCP")]
    FailedToSerializeProfile(String),
    #[error("Failed to send profile data for transmitting to GCP")]
    FailedToSendProfileToGCP(String),
}

/// This is a best effort attempt to run the GCP profiler on a rust
/// service. This is not officially supported by Google Cloud and
/// can run the risk of breaking at some point.
///
/// # Example
///
/// ```
/// use cloud_profiler_rust;
/// cloud_profiler_rust::maybe_start_profiling("my-service", "v1", || { should_run_profiler() });
/// ```
pub async fn maybe_start_profiling<F>(service: String, version: String, should_start: F)
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    if !on_gce().await {
        return;
    }

    let shared_should_start = Arc::new(should_start);
    tokio::spawn(async move {
        // Define constants
        let mut labels = HashMap::new();
        labels.insert("language".to_string(), "go".to_string());
        labels.insert("version".to_string(), version.clone());
        let deployment = Some(Deployment {
            project_id: Some("statsig-services".to_string()),
            target: Some(service.clone()),
            labels: Some(labels),
        });

        loop {
            if !shared_should_start() {
                // Sleep for 60 seconds
                tokio::time::sleep(std::time::Duration::new(60, 0)).await;
                continue;
            }

            // Auth: Re-fetch auth token on every loop just incase we are
            //       using GCP Metadata server to get the token.
            let token = match get_auth_token().await {
                Ok(token) => token,
                Err(e) => {
                    println!("[gcp cloud profiler] Error getting auth token: {:?}", e);
                    return;
                }
            };
            // Create client for communicating with GCP profiler server
            let hub = CloudProfiler::new(
                hyper::Client::builder().build(
                    hyper_rustls::HttpsConnectorBuilder::new()
                        .with_native_roots()
                        .https_or_http()
                        .enable_http1()
                        .build(),
                ),
                token,
            );

            // Make a request to GCP profiler server to generate
            // a new profile instance
            let profile = match create_profile(&hub, &deployment).await {
                Ok(profile) => profile,
                Err(e) => {
                    // TODO: retry if creation fails with exponential backoff
                    println!("[gcp cloud profiler] Error creating profile: {:?}", e);
                    return;
                }
            };
            let profile_duration = match profile.duration {
                Some(d) => std::time::Duration::new(
                    d.num_seconds() as u64,
                    (d.num_milliseconds() as u32) * 1000,
                ),
                None => {
                    println!("[gcp cloud profiler] Profile missing duration...");
                    continue;
                }
            };

            // Profile application using pprof based on the duration
            // specified by the GCP profiler server
            let report = match do_profile(profile_duration).await {
                Ok(report) => report,
                Err(e) => {
                    println!("[gcp cloud profiler] Error profiling: {:?}", e);
                    return;
                }
            };
            // Send profiled data to GCP profiler server
            if let Err(e) = update_gcp_profile_server(&hub, report, profile).await {
                println!("[gcp cloud profiler] Error updating profile: {:?}", e);
                return;
            }
        }
    });
}

async fn get_auth_token() -> Result<String, GcpCloudProfilingError> {
    let tsp = google_cloud_auth::token::DefaultTokenSourceProvider::new(
        google_cloud_auth::project::Config {
            audience: None,
            scopes: Some(&SCOPES),
            sub: None,
        },
    )
    .await
    .map_err(|e| GcpCloudProfilingError::FailedToGetAuthToken(e.to_string()))?;
    let token = tsp
        .token_source()
        .token()
        .await
        .map_err(|e| GcpCloudProfilingError::FailedToGetAuthToken(e.to_string()))?;
    Ok(token.trim_start_matches("Bearer ").to_string())
}

async fn create_profile(
    hub: &CloudProfiler<HttpsConnector<HttpConnector>>,
    deployment: &Option<Deployment>,
) -> Result<Profile, GcpCloudProfilingError> {
    let request = CreateProfileRequest {
        deployment: deployment.clone(),
        profile_type: Some(vec!["Wall".to_string()]),
    };
    match hub
        .projects()
        .profiles_create(request, "projects/statsig-services")
        .doit()
        .await
    {
        Ok((_response, profile)) => Ok(profile),
        Err(e) => Err(GcpCloudProfilingError::FailedToCreateProfile(e.to_string())),
    }
}

async fn do_profile(profile_duration: Duration) -> Result<Report, GcpCloudProfilingError> {
    let guard = match pprof::ProfilerGuard::new(1000) {
        Ok(guard) => guard,
        Err(e) => {
            return Err(GcpCloudProfilingError::FailedToProfileApplication(
                e.to_string(),
            ));
        }
    };
    tokio::time::sleep(profile_duration).await;
    guard
        .report()
        .build()
        .map_err(|e| GcpCloudProfilingError::FailedToBuildReport(e.to_string()))
}

async fn update_gcp_profile_server(
    hub: &CloudProfiler<HttpsConnector<HttpConnector>>,
    report: Report,
    mut profile: Profile,
) -> Result<(), GcpCloudProfilingError> {
    match report.pprof() {
        Ok(pprof_data) => {
            // Gzip the data before sending it to GCP
            let mut content = Vec::new();
            if let Err(e) = pprof_data.write_to_vec(&mut content) {
                return Err(GcpCloudProfilingError::FailedToSerializeProfile(
                    e.to_string(),
                ));
            }
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&content).unwrap();
            let compressed_content = encoder.finish().unwrap();

            // Send profile data to GCP
            profile.profile_bytes = Some(compressed_content);
            let name = match profile.name.clone() {
                Some(name) => name,
                None => {
                    return Err(GcpCloudProfilingError::FailedToSerializeProfile(
                        "GCP profile did not contain a name...".to_string(),
                    ));
                }
            };
            if let Err(e) = hub.projects().profiles_patch(profile, &name).doit().await {
                return Err(GcpCloudProfilingError::FailedToSendProfileToGCP(
                    e.to_string(),
                ));
            }
        }
        Err(e) => {
            return Err(GcpCloudProfilingError::FailedToSerializeProfile(
                e.to_string(),
            ));
        }
    }

    Ok(())
}
