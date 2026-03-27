/// Stream names for a tenant's NATS `JetStream` resources.
pub struct TenantStreams {
    /// Telemetry stream name.
    pub telemetry: String,
    /// Events stream name.
    pub events: String,
    /// Tasks stream name.
    pub tasks: String,
}

impl TenantStreams {
    /// Derive conventional stream names for a given tenant.
    pub fn for_tenant(tenant_id: &str) -> Self {
        Self {
            telemetry: format!("TELEMETRY_{tenant_id}"),
            events: format!("EVENTS_{tenant_id}"),
            tasks: format!("TASKS_{tenant_id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_stream_names() {
        let streams = TenantStreams::for_tenant("tenant-abc");
        assert_eq!(streams.telemetry, "TELEMETRY_tenant-abc");
        assert_eq!(streams.events, "EVENTS_tenant-abc");
        assert_eq!(streams.tasks, "TASKS_tenant-abc");
    }
}
