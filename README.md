# cloud-profiler-rust

![version](https://img.shields.io/crates/v/cloud_profiler_rust.svg)
![downloads](https://img.shields.io/crates/d/cloud_profiler_rust.svg)

This library was created based on the google support implementations:
- cloud-profiler-go
- cloud-profiler-node
- etc.

As a result of this being completed by examining those other libraries. This is not officially supported by Google at this point in time.

That being said, it seems to work as long as you don't give away the fact that we are actually a rust binary and not go, per:

https://github.com/statsig-io/cloud-profiler-rust/blob/main/src/lib.rs#L64

# Usage

Using this library is extremely straight forward, this is an example from our [forward proxy](https://github.com/statsig-io/statsig-forward-proxy/tree/main), that uses
both a static enablement and dynamic enablement method:

```
cloud_profiler_rust::maybe_start_profiling(
        "statsig-forward-proxy".to_string(),
        std::env::var("DD_VERSION").unwrap_or("missing_dd_version".to_string()),
        move || {
            force_enable
                || Statsig::check_gate(&statsig_user, "enable_gcp_profiler_for_sfp").unwrap_or(false)
        },
    )
    .await;
```
