use std::sync::Arc;
use std::time::Duration;

use sysinfo::{System,ProcessesToUpdate};
use tokio::task::JoinHandle;
use wasmcloud_tracing::{
    Counter, Gauge, Histogram, KeyValue, Meter, ObservableGauge, UpDownCounter,
};

const DEFAULT_REFRESH_TIME: Duration = Duration::from_secs(5);

/// `HostMetrics` encapsulates the set of metrics emitted by the wasmcloud host
#[derive(Clone, Debug)]
#[allow(clippy::module_name_repetitions)]
pub struct HostMetrics {
    /// Represents the time it took for each handle_rpc_message invocation in nanoseconds.
    pub handle_rpc_message_duration_ns: Histogram<u64>,
    /// The count of the number of times an component was invoked.
    pub component_invocations: Counter<u64>,
    /// The count of the number of times an component invocation resulted in an error.
    pub component_errors: Counter<u64>,
    /// The number of active instances of a component.
    pub component_active_instances: UpDownCounter<i64>,
    /// The maximum number of instances of a component.
    pub component_max_instances: Gauge<u64>,

    /// The total amount of available system memory in bytes.
    pub system_total_memory_bytes: ObservableGauge<u64>,
    /// Used system memory in bytes of the host.
    pub host_used_memory_bytes: ObservableGauge<u64>,
    /// The total cpu usage of the host.
    pub host_cpu_usage: ObservableGauge<f64>,

    /// The host ID.
    pub host_id: String,

    /// The host lattice ID.
    pub lattice_id: String,

    // Task handle for dropping when the metrics are no longer needed.
    _refresh_task_handle: Arc<RefreshWrapper>,
}

struct SystemMetrics {
    system_total_memory_bytes: u64,
    host_used_memory_bytes: u64,
    host_cpu_usage: f64,
}

/// A helper struct for encapsulating the system metrics that should be wrapped in an Arc.
///
/// When the final reference is removed, the drop will abort the watch task. This allows the metrics
/// to be clonable
#[derive(Debug)]
struct RefreshWrapper(JoinHandle<()>);

impl Drop for RefreshWrapper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl HostMetrics {
    /// Construct a new [`HostMetrics`] instance for accessing the various wasmcloud host metrics
    /// linked to the provided meter.
    ///
    /// The `refresh_time` is optional and defaults to 5 seconds. This time is used to configure how
    /// often system level metrics are refreshed
    pub fn new(
        meter: &Meter,
        host_id: String,
        lattice_id: String,
        refresh_time: Option<Duration>,
    ) -> anyhow::Result<Self> {
        let wasmcloud_host_handle_rpc_message_duration_ns = meter
            .u64_histogram("wasmcloud_host.handle_rpc_message.duration")
            .with_description("Duration in nanoseconds each handle_rpc_message operation took")
            .with_unit("nanoseconds")
            .build();

        let component_invocation_count = meter
            .u64_counter("wasmcloud_host.component.invocations")
            .with_description("Number of component invocations")
            .build();

        let component_error_count = meter
            .u64_counter("wasmcloud_host.component.invocation.errors")
            .with_description("Number of component errors")
            .build();

        let component_active_instances = meter
            .i64_up_down_counter("wasmcloud_host.component.active_instances")
            .with_description("Number of active component instances")
            .build();

        let component_max_instances = meter
            .u64_gauge("wasmcloud_host.component.max_instances")
            .with_description("Maximum number of component instances")
            .build();

        // Initialize sysinfo to read system metrics
        let mut system = System::new();

        // Refresh total system memory
        system.refresh_memory();
        system.refresh_cpu();

        // Get the current process PID
        let pid = sysinfo::get_current_pid().map_err(
            |e| anyhow::anyhow!("Failed to get current pid: {:?}", e)
        )?;

        // Refresh processes and initial memory
        system.refresh_processes(ProcessesToUpdate::All, true);
        
        // Get the current process info
        let process = system.process(pid).ok_or_else(|| {
            anyhow::anyhow!("Process with pid {:?} not found during metrics initialization", pid)
        })?;

        // Initialize the metrics
        let initial_metrics = SystemMetrics {
            system_total_memory_bytes: system.total_memory(),
            host_used_memory_bytes: process.memory(),
            host_cpu_usage: process.cpu_usage() as f64,
        };

        // Watch channel to share metrics between refresh task and the gauges
        let (tx, rx) = tokio::sync::watch::channel(initial_metrics);

        let refresh_time = refresh_time.unwrap_or(DEFAULT_REFRESH_TIME);

        // Async task that periodically refreshes metrics
        let refresh_task_handle = tokio::spawn(async move {
            loop {
                system.refresh_memory();
                system.refresh_cpu();
                system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                
                if let Some(proc) = system.process(pid) {
                    tx.send_modify(|current| {
                        current.system_total_memory_bytes = system.total_memory();

                        current.host_used_memory_bytes = proc.memory();
                        current.host_cpu_usage = proc.cpu_usage() as f64;
                    });
                }
                tokio::time::sleep(refresh_time).await;
            }
        });

        // Observable gauge for total system memory
        let system_memory_total_bytes = meter
            .u64_observable_gauge("wasmcloud_host.process.memory.total.bytes")
            .with_description("The total amount of memory in bytes")
            .with_unit("bytes")
            .with_callback({
                let rx = rx.clone();
                move |observer| {
                    let metrics = rx.borrow();
                    observer.observe(metrics.system_total_memory_bytes, &[]);
                }
            })
            .build();
        
        // Clone host_id and lattice_id for use in callbacks
        let host_id_clone = host_id.clone();
        let lattice_id_clone = lattice_id.clone();
        
         // Observable gauge for host used memory
        let host_memory_used_bytes = meter
            .u64_observable_gauge("wasmcloud_host.process.memory.used.bytes")
            .with_description("The used amount of memory in bytes")
            .with_unit("bytes")
            .with_callback({
                let rx_clone = rx.clone();
                let host_id = host_id_clone.clone();
                let lattice_id = lattice_id_clone.clone();
                move |observer| {
                    let metrics = rx_clone.borrow();
                    observer.observe(metrics.host_used_memory_bytes, &[
                        KeyValue::new("host_id", host_id.clone()),
                        KeyValue::new("lattice_id", lattice_id.clone()),
                    ]);
                }
            })
            .build();

        // Observable gauge for host CPU usage
        let host_cpu_usage = meter
            .f64_observable_gauge("wasmcloud_host.process.cpu.usage")
            .with_description("The CPU usage of the process")
            .with_unit("percentage")
            .with_callback({
                let rx = rx.clone();
                let host_id = host_id_clone.clone();
                let lattice_id = lattice_id_clone.clone();
                move |observer| {
                    let metrics = rx.borrow();
                    observer.observe(metrics.host_cpu_usage, &[
                        KeyValue::new("host_id", host_id.clone()),
                        KeyValue::new("lattice_id", lattice_id.clone()),
                    ]);
                }
            })
            .build();

        Ok(Self {
            handle_rpc_message_duration_ns: wasmcloud_host_handle_rpc_message_duration_ns,
            component_invocations: component_invocation_count,
            component_errors: component_error_count,
            component_active_instances,
            component_max_instances,
            system_total_memory_bytes: system_memory_total_bytes,
            host_used_memory_bytes: host_memory_used_bytes,
            host_cpu_usage: host_cpu_usage,
            host_id,
            lattice_id,
            _refresh_task_handle: Arc::new(RefreshWrapper(refresh_task_handle)),
        })
    }

    /// Increment the number of active instances of a component.
    pub(crate) fn increment_active_instance(&self, attributes: &[KeyValue]) {
        self.component_active_instances.add(1, attributes);
    }

    /// Decrement the number of active instances of a component.
    pub(crate) fn decrement_active_instance(&self, attributes: &[KeyValue]) {
        self.component_active_instances.add(-1, attributes);
    }

    /// Set the maximum number of instances of a component.
    pub(crate) fn set_max_instances(&self, max: u64, attributes: &[KeyValue]) {
        self.component_max_instances.record(max, attributes);
    }

    /// Record the result of invoking a component, including the elapsed time, any attributes, and whether the invocation resulted in an error.
    pub(crate) fn record_component_invocation(
        &self,
        elapsed: u64,
        attributes: &[KeyValue],
        error: bool,
    ) {
        self.handle_rpc_message_duration_ns
            .record(elapsed, attributes);
        self.component_invocations.add(1, attributes);
        if error {
            self.component_errors.add(1, attributes);
        }
    }
}
