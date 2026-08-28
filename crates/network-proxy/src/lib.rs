//! P2/TASK-203：默认拒绝、精确域名白名单的 CONNECT 网络代理。

mod policy;
mod server;

pub use policy::ProxyPolicy;
pub use server::{AuditSink, ProxyServer};
