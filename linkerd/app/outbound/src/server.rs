use crate::{http, stack_labels, tcp, trace_labels, Config};
use linkerd2_app_core::{
    config::{ProxyConfig, ServerConfig},
    discovery_rejected, drain, errors, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{api_resolve::Metadata, core::resolve::Resolve},
    spans::SpanConverter,
    svc,
    transport::{self, io, listen, tls},
    Addr, Error, IpMatch, TraceContext,
};
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tracing::debug_span;

pub fn stack<R, P, C, H, S, I>(
    config: &Config,
    profiles: P,
    resolve: R,
    tcp_connect: C,
    http_router: H,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    R::Future: Unpin + Send,
    R::Resolution: Unpin + Send,
    C: tower::Service<tcp::Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
    H: svc::NewService<http::Logical, Service = S> + Unpin + Clone + Send + Sync + 'static,
    S: tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    S::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + Sync + 'static,
    P::Future: Unpin + Send,
    P::Error: Send,
{
    let tcp_balance = tcp::balance::stack(&config.proxy, tcp_connect.clone(), resolve);
    let accept = accept_with_tcp_balancer(
        config,
        profiles,
        tcp_connect,
        tcp_balance,
        http_router,
        metrics.clone(),
        span_sink,
        drain,
    );
    cache_accept(&config.proxy, metrics, accept)
}

pub fn cache_accept<N, S, I>(
    config: &ProxyConfig,
    metrics: metrics::Proxy,
    stack: N,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    transport::metrics::SensorIo<I>: Send + 'static,
    N: svc::NewService<tcp::Accept, Service = S> + Clone + Send + 'static,
    S: svc::Service<transport::metrics::SensorIo<I>, Response = ()> + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Future: Send + 'static,
{
    svc::stack(stack)
        .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
        .cache(
            svc::layers().push_on_response(
                svc::layers()
                    .push_failfast(config.dispatch_timeout)
                    .push_spawn_buffer_with_idle_timeout(
                        config.buffer_capacity,
                        config.cache_max_idle_age,
                    ),
            ),
        )
        .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
        .push(metrics.transport.layer_accept())
        .push_map_target(tcp::Accept::from)
        .check_new_service::<listen::Addrs, I>()
        .into_inner()
}

pub fn accept_with_tcp_balancer<P, C, T, H, S, I>(
    config: &Config,
    profiles: P,
    tcp_connect: C,
    tcp_balance: T,
    http_router: H,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    tcp::Accept,
    Service = impl tower::Service<
        transport::metrics::SensorIo<I>,
        Response = (),
        Error = impl Into<Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Clone + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    C: tower::Service<tcp::Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
    T: svc::NewService<tcp::Concrete> + Clone + Unpin + Send + 'static,
    T::Service: tower::Service<transport::io::PrefixedIo<transport::metrics::SensorIo<I>>, Response = (), Error = Error> + Unpin + Send + 'static,
    <T::Service as tower::Service<transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>>::Future: Unpin + Send + 'static,
    H: svc::NewService<http::Logical, Service = S> + Unpin + Clone + Send + Sync + 'static,
    S: tower::Service<
            http::Request<http::boxed::BoxBody>,
            Response = http::Response<http::boxed::BoxBody>,
            Error = Error,
        > + Send
        + 'static,
    S::Future: Send,
    P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + Sync + 'static,
    P::Future: Unpin + Send,
    P::Error: Send,
{
    let ProxyConfig {
        server: ServerConfig { h2_settings, .. },
        dispatch_timeout,
        max_in_flight_requests,
        detect_protocol_timeout,
        cache_max_idle_age,
        buffer_capacity,
        ..
    } = config.proxy.clone();

    let http_server = svc::stack(http_router)
        .check_new_service::<http::Logical, http::Request<_>>()
        .push_on_response(
            svc::layers()
                .box_http_request()
                // Limits the number of in-flight requests.
                .push_concurrency_limit(max_in_flight_requests)
                // Eagerly fail requests when the proxy is out of capacity for a
                // dispatch_timeout.
                .push_failfast(dispatch_timeout)
                .push(metrics.http_errors.clone())
                // Synthesizes responses for proxy errors.
                .push(errors::layer())
                // Initiates OpenCensus tracing.
                .push(TraceContext::layer(span_sink.clone().map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.stack.layer(stack_labels("source")))
                .push_failfast(dispatch_timeout)
                .push_spawn_buffer(buffer_capacity)
                .box_http_response(),
        )
        .check_new_service::<http::Logical, http::Request<_>>()
        .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
        .instrument(|l: &http::Logical| debug_span!("http", v = %l.protocol))
        .push_map_target(http::Logical::from)
        .check_new_service::<(http::Version, tcp::Logical), http::Request<_>>()
        .into_inner();

    let tcp_forward = svc::stack(tcp_connect.clone())
        .push_make_thunk()
        .check_make_service::<tcp::Endpoint, ()>()
        .push_on_response(svc::layer::mk(tcp::Forward::new))
        .into_new_service()
        .check_new_service::<tcp::Endpoint, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
        .push_map_target(tcp::Endpoint::from_logical(
            tls::ReasonForNoPeerName::NotProvidedByServiceDiscovery,
        ))
        .check_new_service::<tcp::Logical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
        .into_inner();

    // Load balances TCP streams that cannot be decoded as HTTP.
    let tcp_balance = svc::stack(tcp_balance)
    .push_map_target(tcp::Concrete::from)
    .push(profiles::split::layer())
    .check_new_service::<tcp::Logical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
    .push_switch(tcp::Logical::should_resolve, tcp_forward)
    .push_on_response(
        svc::layers()
            .push_failfast(dispatch_timeout)
            .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
    )
    .instrument(|_: &_| debug_span!("tcp"))
    .check_new_service::<tcp::Logical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
    .into_inner();

    let http = svc::stack(http::NewServeHttp::new(
        h2_settings,
        http_server,
        tcp_balance,
        drain.clone(),
    ))
    .check_new_service::<tcp::Logical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
    .push_on_response(svc::layers().push_spawn_buffer(buffer_capacity).push(
        transport::Prefix::layer(
            http::Version::DETECT_BUFFER_CAPACITY,
            detect_protocol_timeout,
        ),
    ))
    .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
    .into_inner();

    let tcp = svc::stack(tcp::connect::forward(tcp_connect))
        .push_map_target(tcp::Endpoint::from_logical(
            tls::ReasonForNoPeerName::PortSkipped,
        ))
        .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
        .into_inner();

    svc::stack(http)
        .push_switch(SkipByProfile, tcp)
        .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
        .push_map_target(tcp::Logical::from)
        .push(profiles::discover::layer(
            profiles,
            AllowProfile(config.allow_discovery.clone().into()),
        ))
        .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
        .into_inner()
}

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::FilterRequest<tcp::Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn filter(&self, a: tcp::Accept) -> Result<std::net::SocketAddr, Error> {
        if self.0.matches(a.orig_dst.ip()) {
            Ok(a.orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl SkipByProfile ===

impl svc::stack::Switch<tcp::Logical> for SkipByProfile {
    fn use_primary(&self, l: &tcp::Logical) -> bool {
        l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
    }
}
