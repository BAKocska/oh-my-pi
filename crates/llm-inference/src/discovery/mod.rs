//! Active endpoint discovery, restart-safe caching, and account catalog
//! merging.

pub mod accounts;
pub mod endpoints;
pub mod probe;
pub mod store;

pub use accounts::{AccountCatalog, AccountDiscoveredModel, merge_account_catalogs};
pub use endpoints::{
	DiscoveryEndpoint, DiscoveryEndpointKind, EndpointError, EndpointOrigin, configured_endpoint,
	known_loopback_endpoints, supported_endpoint_types,
};
pub use probe::{
	DiscoveryHttpClient, DiscoveryProbe, ProbeError, ProbeHttpFuture, ProbeHttpRequest,
};
pub use store::{
	CachedDiscovery, DiscoveryStore, DiscoveryStoreError, ProviderDiscoveryState, ProviderLifecycle,
};
