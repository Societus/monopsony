use std::net::IpAddr;
use std::sync::Mutex;
use tracing::{debug, info, warn};

pub struct DiscoveryService {
    service_name: String,
    http_port: u16,
    grpc_port: u16,
    state: Mutex<DiscoveryState>,
}

struct DiscoveryState {
    daemon: Option<mdns_sd::ServiceDaemon>,
    service_fullname: Option<String>,
}

impl DiscoveryService {
    pub fn new(service_name: &str, http_port: u16, grpc_port: u16) -> anyhow::Result<Self> {
        Ok(Self {
            service_name: service_name.to_string(),
            http_port,
            grpc_port,
            state: Mutex::new(DiscoveryState {
                daemon: None,
                service_fullname: None,
            }),
        })
    }

    pub async fn start(&self, addr: IpAddr) -> anyhow::Result<()> {
        let service_type = format!("_{}._tcp.local.", self.service_name);
        let instance_name = "monopsony";

        let fullname = format!("{}.{}", instance_name, service_type);
        let host_name = format!("{}.local.", addr);

        let properties = [
            ("http_port", self.http_port.to_string()),
            ("grpc_port", self.grpc_port.to_string()),
            ("version", "0.1.0".to_string()),
        ];

        let service_info = mdns_sd::ServiceInfo::new(
            &service_type,
            instance_name,
            &host_name,
            addr,
            self.http_port,
            &properties[..],
        )?;

        let daemon = mdns_sd::ServiceDaemon::new()?;
        daemon.register(service_info)?;

        info!(
            service_type = %service_type,
            addr = %addr,
            http_port = self.http_port,
            grpc_port = self.grpc_port,
            "mDNS service registered"
        );

        let mut state = self.state.lock().unwrap();
        state.daemon = Some(daemon);
        state.service_fullname = Some(fullname);

        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();

        if let Some(fullname) = state.service_fullname.take() {
            if let Some(daemon) = &state.daemon {
                match daemon.unregister(&fullname) {
                    Ok(receiver) => {
                        match receiver.recv_timeout(std::time::Duration::from_secs(2)) {
                            Ok(mdns_sd::UnregisterStatus::OK) => {
                                debug!(fullname = %fullname, "mDNS service unregistered");
                            }
                            Ok(mdns_sd::UnregisterStatus::NotFound) => {
                                warn!(fullname = %fullname, "mDNS service not found for unregister");
                            }
                            Err(e) => {
                                warn!(error = %e, "timeout waiting for mDNS unregister");
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to unregister mDNS service");
                    }
                }
            }
        }

        if let Some(daemon) = state.daemon.take() {
            match daemon.shutdown() {
                Ok(receiver) => {
                    match receiver.recv_timeout(std::time::Duration::from_secs(2)) {
                        Ok(mdns_sd::DaemonStatus::Shutdown) => {
                            info!("mDNS daemon shut down");
                        }
                        Ok(status) => {
                            warn!(?status, "unexpected mDNS daemon status on shutdown");
                        }
                        Err(e) => {
                            warn!(error = %e, "timeout waiting for mDNS daemon shutdown");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "failed to shutdown mDNS daemon");
                }
            }
        }

        Ok(())
    }
}

impl Drop for DiscoveryService {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap();
        if state.daemon.is_some() || state.service_fullname.is_some() {
            if let Some(daemon) = state.daemon.take() {
                if let Some(fullname) = state.service_fullname.take() {
                    let _ = daemon.unregister(&fullname);
                }
                let _ = daemon.shutdown();
            }
        }
    }
}
