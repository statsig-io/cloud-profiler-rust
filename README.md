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

## Features

This library now supports both **CPU profiling** and **Heap profiling** using jemalloc:

- **CPU Profiling**: Traditional CPU sampling profiling using the `pprof` crate
- **Heap Profiling**: Complete memory allocation profiling using `tikv-jemalloc`
  
For more details on how heap profiling works internally, see the [rust-jemalloc-pprof](https://github.com/polarsignals/rust-jemalloc-pprof) documentation.

## Configuring Heap Profiling Behavior

Users can override the default jemalloc configuration using environment variables if needed:

```bash
export MALLOC_CONF="prof:true,prof_active:false,lg_prof_sample:21"
```

# Usage

## Basic Usage

The cloud profiler will **always perform CPU profiling** when enabled. You can optionally enable heap profiling as well.

```rust
use cloud_profiler_rust::CloudProfilerConfiguration;

cloud_profiler_rust::maybe_start_profiling(
    "my-project-id".to_string(),
    "my-service".to_string(), 
    "v1.0.0".to_string(),
    move || {
        force_enable
            || Statsig::check_gate(&statsig_user, "enable_gcp_profiler").unwrap_or(false)
    },
    move || CloudProfilerConfiguration {
        sampling_rate: 100,
        heap_profile_active: true, // Enable heap profiling
    }
).await;
```
