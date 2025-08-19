mod backoff;
use flate2::write::GzEncoder;
use flate2::Compression;
use google_cloud_metadata::on_gce;
use google_cloud_token::TokenSourceProvider;
use google_cloudprofiler2::api::CreateProfileRequest;
use google_cloudprofiler2::api::Deployment;
use google_cloudprofiler2::api::Profile;
use google_cloudprofiler2::hyper::client::HttpConnector;
use google_cloudprofiler2::{hyper, CloudProfiler};
use hyper_rustls::HttpsConnector;
use pprof::protos::Message;
use pprof::Report;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::default::Default;

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

// Configure jemalloc for heap profiling
// Based on rust-jemalloc-pprof recommended settings
#[cfg(not(target_env = "msvc"))]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

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
    #[error("Failed to enable jemalloc heap profiling")]
    FailedToEnableHeapProfiling(String),
    #[error("Failed to dump jemalloc heap profile")]
    FailedToDumpHeapProfile(String),
}

#[derive(Serialize, Deserialize, Clone)]
pub enum ProfileType {
    CPU,
    Heap,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CloudProfilerConfiguration {
    pub sampling_rate: i32,
    pub heap_profile_active: bool,
}

/// This is a best effort attempt to run the GCP profiler on a rust
/// service. This is not officially supported by Google Cloud and
/// can run the risk of breaking at some point.
///
/// # Example
///
/// ```
/// use cloud_profiler_rust;
/// cloud_profiler_rust::maybe_start_profiling("my-gcp-project-id", "my-service", "v1", || { should_run_profiler() });
/// ```
pub async fn maybe_start_profiling<F, G>(
    project_id: String,
    service: String,
    version: String,
    should_start: F,
    get_configuration: G,
) where
    F: Fn() -> bool + Send + Sync + 'static,
    G: Fn() -> CloudProfilerConfiguration + Send + Sync + 'static,
{
    if !on_gce().await {
        return;
    }

    let shared_should_start = Arc::new(should_start);
    let shared_get_configuration = Arc::new(get_configuration);
    tokio::spawn(async move {
        // Define constants
        let mut labels = HashMap::new();
        labels.insert("language".to_string(), "go".to_string());
        labels.insert("version".to_string(), version.clone());
        let deployment = Some(Deployment {
            project_id: Some(project_id),
            target: Some(service.clone()),
            labels: Some(labels),
        });

        let mut backoff_provider = backoff::Backoff::new(60.0, 3600.0, 1.3);
        let mut retry_back_off = None;
        loop {
            if !shared_should_start() {
                // Sleep for 60 seconds
                tokio::time::sleep(std::time::Duration::new(60, 0)).await;
                continue;
            }
            if let Some(rbo) = retry_back_off {
                println!("[gcp cloud profiler] Retrying in {:.3} seconds...", rbo);
                tokio::time::sleep(std::time::Duration::from_secs_f64(rbo)).await;
            } else {
                // Reset backoff if we're succeeding
                backoff_provider = backoff::Backoff::new(60.0, 3600.0, 1.3);
                retry_back_off = None;
            }

            let configuration = shared_get_configuration();
            
            // Always do CPU profiling
            let cpu_profile = match create_profile(&deployment, &ProfileType::CPU).await {
                Ok(profile) => profile,
                Err(e) => {
                    println!("[gcp cloud profiler] Error creating CPU profile: {:?}", e);
                    retry_back_off = Some(backoff_provider.next_backoff());
                    continue;
                }
            };
            let cpu_profile_duration = match cpu_profile.duration {
                Some(d) => std::time::Duration::new(
                    d.num_seconds() as u64,
                    (d.num_milliseconds() as u32) * 1000,
                ),
                None => {
                    println!("[gcp cloud profiler] CPU profile missing duration...");
                    retry_back_off = Some(backoff_provider.next_backoff());
                    continue;
                }
            };

            // Do CPU profiling
            let cpu_profile_data = match do_cpu_profile(cpu_profile_duration, &configuration).await {
                Ok(report) => ProfileData::CPU(report),
                Err(e) => {
                    println!("[gcp cloud profiler] Error CPU profiling: {:?}", e);
                    retry_back_off = Some(backoff_provider.next_backoff());
                    continue;
                }
            };
            
            // Send CPU profile data to GCP
            if let Err(e) = update_gcp_profile_server(cpu_profile_data, cpu_profile).await {
                println!("[gcp cloud profiler] Error updating CPU profile: {:?}", e);
                retry_back_off = Some(backoff_provider.next_backoff());
                continue;
            }

            if configuration.heap_profile_active {
                let heap_profile = match create_profile(&deployment, &ProfileType::Heap).await {
                    Ok(profile) => profile,
                    Err(e) => {
                        println!("[gcp cloud profiler] Error creating heap profile: {:?}", e);
                        continue;
                    }
                };
                let heap_profile_duration = match heap_profile.duration {
                    Some(d) => std::time::Duration::new(
                        d.num_seconds() as u64,
                        (d.num_milliseconds() as u32) * 1000,
                    ),
                    None => {
                        println!("[gcp cloud profiler] Heap profile missing duration...");
                        continue;
                    }
                };

                let heap_profile_data = match do_heap_profile_raw(heap_profile_duration, &configuration).await {
                    Ok(pprof_data) => ProfileData::Heap(pprof_data),
                    Err(e) => {
                        println!("[gcp cloud profiler] Error heap profiling: {:?}", e);
                        continue;
                    }
                };
                
                if let Err(e) = update_gcp_profile_server(heap_profile_data, heap_profile).await {
                    println!("[gcp cloud profiler] Error updating heap profile: {:?}", e);
                    // Don't fail the entire loop for heap profiling errors
                }
            }
        }
    });
}

async fn get_hub() -> Result<CloudProfiler<HttpsConnector<HttpConnector>>, GcpCloudProfilingError> {
    // Auth: Re-fetch auth token on every loop just incase we are
    //       using GCP Metadata server to get the token.
    let token = get_auth_token().await?;
    // Create client for communicating with GCP profiler server
    Ok(CloudProfiler::new(
        hyper::Client::builder().build(
            hyper_rustls::HttpsConnectorBuilder::new()
                .with_native_roots()
                .https_or_http()
                .enable_http1()
                .build(),
        ),
        token,
    ))
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
    deployment: &Option<Deployment>,
    profile_type: &ProfileType,
) -> Result<Profile, GcpCloudProfilingError> {
    let profile_type_str = match profile_type {
        ProfileType::CPU => "CPU",
        ProfileType::Heap => "HEAP",
    };
    
    let request = CreateProfileRequest {
        deployment: deployment.clone(),
        profile_type: Some(vec![profile_type_str.to_string()]),
    };
    match get_hub()
        .await?
        .projects()
        .profiles_create(request, "projects/statsig-services")
        .doit()
        .await
    {
        Ok((_response, profile)) => Ok(profile),
        Err(e) => Err(GcpCloudProfilingError::FailedToCreateProfile(e.to_string())),
    }
}



enum ProfileData {
    CPU(Report),
    Heap(Vec<u8>),
}

async fn do_cpu_profile(
    profile_duration: Duration,
    configuration: &CloudProfilerConfiguration,
) -> Result<Report, GcpCloudProfilingError> {
    let guard = match pprof::ProfilerGuard::new(configuration.sampling_rate) {
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

async fn do_heap_profile_raw(
    profile_duration: Duration,
    configuration: &CloudProfilerConfiguration,
) -> Result<Vec<u8>, GcpCloudProfilingError> {
    if configuration.heap_profile_active {
        jemalloc_profiling::set_prof_active(true)?;
    }

    tokio::time::sleep(profile_duration).await;

    // pprof in protobuf
    let pprof_data = jemalloc_profiling::dump_heap_profile()?;

    // Disable heap profiling if it was enabled
    if configuration.heap_profile_active {
        jemalloc_profiling::set_prof_active(false)?;
    }

    Ok(pprof_data)
}



async fn update_gcp_profile_server(
    profile_data: ProfileData,
    mut profile: Profile,
) -> Result<(), GcpCloudProfilingError> {
    let content = match profile_data {
        ProfileData::CPU(report) => {
            match report.pprof() {
                Ok(pprof_data) => {
                    let mut content = Vec::new();
                    if let Err(e) = pprof_data.write_to_vec(&mut content) {
                        return Err(GcpCloudProfilingError::FailedToSerializeProfile(
                            e.to_string(),
                        ));
                    }
                    content
                }
                Err(e) => {
                    return Err(GcpCloudProfilingError::FailedToSerializeProfile(
                        e.to_string(),
                    ));
                }
            }
        }
        ProfileData::Heap(pprof_data) => {
            pprof_data
        }
    };

    // Gzip the data before sending it to GCP
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
    if let Err(e) = get_hub()
        .await?
        .projects()
        .profiles_patch(profile, &name)
        .doit()
        .await
    {
        return Err(GcpCloudProfilingError::FailedToSendProfileToGCP(
            e.to_string(),
        ));
    }

    Ok(())
}

#[cfg(not(target_env = "msvc"))]
mod jemalloc_profiling {
    use super::GcpCloudProfilingError;

    pub fn set_prof_active(active: bool) -> Result<(), GcpCloudProfilingError> {
        use jemalloc_pprof::PROF_CTL;
        
        if let Some(prof_ctl_ref) = PROF_CTL.as_ref() {
            let mut prof_ctl = prof_ctl_ref.try_lock().map_err(|e| {
                GcpCloudProfilingError::FailedToEnableHeapProfiling(format!(
                    "Failed to lock profiler control: {}",
                    e
                ))
            })?;
            
            if active {
                prof_ctl.activate().map_err(|e| {
                    GcpCloudProfilingError::FailedToEnableHeapProfiling(format!(
                        "Failed to activate profiling: {}",
                        e
                    ))
                })?;
            } else {
                prof_ctl.deactivate().map_err(|e| {
                    GcpCloudProfilingError::FailedToEnableHeapProfiling(format!(
                        "Failed to deactivate profiling: {}",
                        e
                    ))
                })?;
            }
            
            Ok(())
        } else {
            Err(GcpCloudProfilingError::FailedToEnableHeapProfiling(
                "Profiler control not initialized".to_string(),
            ))
        }
    }

    pub fn dump_heap_profile() -> Result<Vec<u8>, GcpCloudProfilingError> {
        use jemalloc_pprof::PROF_CTL;
        
        if let Some(prof_ctl_ref) = PROF_CTL.as_ref() {
            let mut prof_ctl = prof_ctl_ref.try_lock().map_err(|e| {
                GcpCloudProfilingError::FailedToDumpHeapProfile(format!(
                    "Failed to lock profiler control: {}",
                    e
                ))
            })?;
            
            if !prof_ctl.activated() {
                return Err(GcpCloudProfilingError::FailedToDumpHeapProfile(
                    "Heap profiling is not activated".to_string()
                ));
            }
            
            prof_ctl.dump_pprof().map_err(|e| {
                GcpCloudProfilingError::FailedToDumpHeapProfile(format!(
                    "Failed to dump heap profile: {}",
                    e
                ))
            })
        } else {
            Err(GcpCloudProfilingError::FailedToDumpHeapProfile(
                "Profiler control not initialized".to_string(),
            ))
        }
    }
}
