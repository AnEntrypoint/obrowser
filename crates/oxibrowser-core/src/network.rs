//! Network layer — HTTP client, cookie jar, resource loading, IP filtering, robots.txt.

pub mod client;
pub mod cookie;
#[cfg(not(feature = "native"))]
pub mod host_abi;
pub mod intercept;
pub mod ip_filter;
pub mod resource;
pub mod robots;
pub mod ws;

pub use client::HttpClient;
pub use cookie::CookieJar;
pub use intercept::{
    InterceptAction, InterceptedBody, InterceptedResponse, PausedRequest, PausedRequestRegistry,
    SharedRegistry,
};
pub use ip_filter::IpFilter;
pub use robots::RobotStore;

// Re-export wreq Response for use in HttpClient::fetch return type
#[cfg(feature = "native")]
pub use wreq::Response;
