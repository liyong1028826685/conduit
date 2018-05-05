extern crate futures;
extern crate indexmap;
extern crate tower_service;

use futures::{Future, Poll};
use indexmap::IndexMap;
use std::{
    error, fmt, mem,
    collections::VecDeque,
    hash::Hash,
    sync::{Arc, Mutex},
    time::Instant,
};
use tower_service::Service;

pub mod activity;

pub use self::activity::{Activity, HasActivity, TrackActivity, PendingRequest, ActiveResponse};

/// Routes requests based on a configurable `Key`.
pub struct Router<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    inner: Arc<Mutex<Inner<T>>>,
}

/// Provides a strategy for routing a Request to a Service.
///
/// Implementors must provide a `Key` type that identifies each unique route. The
/// `recognize()` method is used to determine the key for a given request. This key is
/// used to look up a route in a cache (i.e. in `Router`), or can be passed to
/// `bind_service` to instantiate the identified route.
pub trait Recognize {
    /// Requests handled by the discovered services
    type Request;

    /// Responses given by the discovered services
    type Response;

    /// Errors produced by the discovered services
    type Error;

    /// Identifies a Route.
    type Key: Clone + Eq + Hash;

    /// Error produced by failed routing
    type RouteError;

    /// A route.
    type Service: Service<
        Request = Self::Request,
        Response = Self::Response,
        Error = Self::Error
    >;

    /// Determines the key for a route to handle the given request.
    fn recognize(&self, req: &Self::Request) -> Option<Self::Key>;

    /// Return a `Service` to handle requests.
    ///
    /// The returned service must always be in the ready state (i.e.
    /// `poll_ready` must always return `Ready` or `Err`).
    fn bind_service(&mut self, key: &Self::Key) -> Result<Self::Service, Self::RouteError>;
}

pub struct Single<S>(Option<S>);

#[derive(Debug, PartialEq)]
pub enum Error<T, U> {
    Inner(T),
    Route(U),
    NoCapacity(usize),
    NotRecognized,
}

pub struct ResponseFuture<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    state: State<T>,
}

type LastUsed<K> = VecDeque<Used<K>>;

struct Inner<T>
where T: Recognize,
{
    routes: IndexMap<T::Key, T::Service>,
    last_used: LastUsed<T::Key>,
    recognize: T,
    capacity: usize,
}

struct Used<K> {
    key: K,
    time: Instant,
}

enum State<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    Inner(<T::Service as Service>::Future),
    RouteError(T::RouteError),
    NoCapacity(usize),
    NotRecognized,
    Invalid,
}

impl<K> From<K> for Used<K> {
    fn from(key: K) -> Self {
        Self {
            key,
            time: Instant::now(),
        }
    }
}

// ===== impl Inner =====

impl<T> Inner<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    fn new(recognize: T, capacity: usize) -> Self {
        Self {
            routes: IndexMap::default(),
            last_used: LastUsed::new(),
            recognize,
            capacity,
        }
    }

    fn current_capacity(&self) -> usize {
        self.capacity - self.routes.len()
    }

    fn create_capacity(&mut self) {
        for idx in 0..self.last_used.len() {
            let idle = self.routes
                .get(&self.last_used[idx].key)
                .map(|r| {
                    let a = r.activity();
                    a.pending_requests() + a.active_responses() > 0
                })
                .unwrap_or(false);

            if idle {
                Self::move_to_end(&mut self.last_used, idx);
                if let Some(Used{key, ..}) = self.last_used.pop_back() {
                    self.routes.remove(&key);
                }
                return;
            }
        }
    }

    fn move_to_end(last_used: &mut LastUsed<T::Key>, idx: usize) {
        for i in idx..last_used.len()-1 {
            last_used.swap(i, i + 1);
        }
    }

    fn mark_route(&mut self, key: &T::Key) -> Option<&mut T::Service> {
        match self.routes.get_mut(key) {
            None => None,
            Some(svc) => {
                Self::mark_used(&mut self.last_used, key);
                Some(svc)
            }
        }
    }

    fn add_route(&mut self, key: T::Key, service: T::Service) {
        Self::mark_used(&mut self.last_used, &key);
        self.routes.insert(key, service);
    }

    fn mark_used(last_used: &mut LastUsed<T::Key>, key: &T::Key) {
        match last_used.iter().rposition(|&Used{key: ref k, ..}| k == key) {
            Some(idx) => {
                last_used[idx].time = Instant::now();
                for i in idx..last_used.len()-1 {
                    last_used.swap(i, i + 1);
                }
            }

            None => {
                last_used.push_back(key.clone().into());
            }
        }
    }
}

// ===== impl Router =====

impl<T> Router<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    pub fn new(recognize: T, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(recognize, capacity))),
        }
    }
}

impl<T> Service for Router<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    type Request = T::Request;
    type Response = T::Response;
    type Error = Error<T::Error, T::RouteError>;
    type Future = ResponseFuture<T>;

    /// Always ready to serve.
    ///
    /// Graceful backpressure is **not** supported at this level, since each request may
    /// be routed to different resources. Instead, requests should be issued and each
    /// route should support a queue of requests.
    ///
    /// TODO Attempt to free capacity in the router.
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    /// Routes the request through an underlying service.
    ///
    /// The response fails when the request cannot be routed.
    fn call(&mut self, request: Self::Request) -> Self::Future {
        let inner = &mut *self.inner.lock().expect("router lock");

        let key = match inner.recognize.recognize(&request) {
            Some(key) => key,
            None => return ResponseFuture::not_recognized(),
        };

        // First, try to load a cached route for `key`.
        if let Some(svc) = inner.mark_route(&key) {
            return ResponseFuture::new(svc.call(request));
        }

        // Since there wasn't a cached route, ensure that there is capacity for a
        // new one.
        if inner.current_capacity() == 0 {
            // If the cache is full, evict the oldest inactive route.
            inner.create_capacity();
        }
        // If all routes are active, fail the request.
        if inner.current_capacity() == 0 {
            return ResponseFuture::no_capacity(inner.capacity);
        }

        // Bind a new route, send the request on the route, and cache the route.
        let mut service = match inner.recognize.bind_service(&key) {
            Ok(service) => service,
            Err(e) => return ResponseFuture::route_err(e),
        };

        let response = service.call(request);
        inner.add_route(key, service);
        ResponseFuture::new(response)
    }
}

impl<T> Clone for Router<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

// ===== impl Single =====

impl<S: Service> Single<S> {
    pub fn new(svc: S) -> Self {
        Single(Some(svc))
    }
}

impl<S: Service> Recognize for Single<S> {
    type Request = S::Request;
    type Response = S::Response;
    type Error = S::Error;
    type Key = ();
    type RouteError = ();
    type Service = S;

    fn recognize(&self, _: &Self::Request) -> Option<Self::Key> {
        Some(())
    }

    fn bind_service(&mut self, _: &Self::Key) -> Result<S, Self::RouteError> {
        Ok(self.0.take().expect("static route bound twice"))
    }
}

// ===== impl ResponseFuture =====

impl<T> ResponseFuture<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    fn new(inner: <T::Service as Service>::Future) -> Self {
        ResponseFuture { state: State::Inner(inner) }
    }

    fn not_recognized() -> Self {
        ResponseFuture { state: State::NotRecognized }
    }

    fn no_capacity(capacity: usize) -> Self {
        ResponseFuture { state: State::NoCapacity(capacity) }
    }

    fn route_err(e: T::RouteError) -> Self {
        ResponseFuture { state: State::RouteError(e) }
    }
}

impl<T> Future for ResponseFuture<T>
where
    T: Recognize,
    T::Service: HasActivity,
{
    type Item = T::Response;
    type Error = Error<T::Error, T::RouteError>;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use self::State::*;

        match self.state {
            Inner(ref mut fut) => fut.poll().map_err(Error::Inner),
            RouteError(..) => {
                match mem::replace(&mut self.state, Invalid) {
                    RouteError(e) => Err(Error::Route(e)),
                    _ => unreachable!(),
                }
            }
            NotRecognized => Err(Error::NotRecognized),
            NoCapacity(capacity) => Err(Error::NoCapacity(capacity)),
            Invalid => panic!(),
        }
    }
}

// ===== impl Error =====

impl<T, U> fmt::Display for Error<T, U>
where
    T: fmt::Display,
    U: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::Inner(ref why) => why.fmt(f),
            Error::Route(ref why) => write!(f, "route recognition failed: {}", why),
            Error::NoCapacity(capacity) => write!(f, "router capacity reached ({})", capacity),
            Error::NotRecognized => f.pad("route not recognized"),
        }
    }
}

impl<T, U> error::Error for Error<T, U>
where
    T: error::Error,
    U: error::Error,
{
    fn cause(&self) -> Option<&error::Error> {
        match *self {
            Error::Inner(ref why) => Some(why),
            Error::Route(ref why) => Some(why),
            _ => None,
        }
    }

    fn description(&self) -> &str {
        match *self {
            Error::Inner(_) => "inner service error",
            Error::Route(_) => "route recognition failed",
            Error::NoCapacity(_) => "router capacity reached",
            Error::NotRecognized => "route not recognized",
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{Poll, Future, future};
    use tower_service::Service;
    use super::{Error, Router};

    struct Recognize;

    struct MultiplyAndAssign(usize, super::TrackActivity);

    enum Request {
        NotRecognized,
        Recgonized(usize),
    }

    impl super::Recognize for Recognize {
        type Request = Request;
        type Response = usize;
        type Error = ();
        type Key = usize;
        type RouteError = ();
        type Service = MultiplyAndAssign;

        fn recognize(&self, req: &Self::Request) -> Option<Self::Key> {
            match *req {
                Request::NotRecognized => None,
                Request::Recgonized(n) => Some(n),
            }
        }

        fn bind_service(&mut self, _: &Self::Key) -> Result<Self::Service, Self::RouteError> {
            Ok(MultiplyAndAssign(1, super::TrackActivity::default()))
        }
    }

    impl Service for MultiplyAndAssign {
        type Request = Request;
        type Response = usize;
        type Error = ();
        type Future = future::FutureResult<usize, ()>;

        fn poll_ready(&mut self) -> Poll<(), ()> {
            unimplemented!()
        }

        fn call(&mut self, req: Self::Request) -> Self::Future {
            let _req = self.1.pending_request();
            let _rsp = self.1.active_response();
            let n = match req {
                Request::NotRecognized => unreachable!(),
                Request::Recgonized(n) => n,
            };
            self.0 *= n;
            future::ok(self.0)
        }
    }

    impl super::HasActivity for MultiplyAndAssign {
        fn activity(&self) -> &super::Activity {
            self.1.activity()
        }
    }

    impl Router<Recognize> {
        fn call_ok(&mut self, req: Request) -> usize {
            self.call(req).wait().expect("should route")
        }

        fn call_err(&mut self, req: Request) -> super::Error<(), ()> {
            self.call(req).wait().expect_err("should not route")
        }
    }

    #[test]
    fn invalid() {
        let mut router = Router::new(Recognize, 1);

        let rsp = router.call_err(Request::NotRecognized);
        assert_eq!(rsp, Error::NotRecognized);
    }

    #[test]
    fn cache_limited_by_capacity() {
        let mut router = Router::new(Recognize, 1);

        let rsp = router.call_ok(Request::Recgonized(2));
        assert_eq!(rsp, 2);

        let rsp = router.call_err(Request::Recgonized(3));
        assert_eq!(rsp, Error::NoCapacity(1));
    }

    #[test]
    fn services_cached() {
        let mut router = Router::new(Recognize, 1);

        let rsp = router.call_ok(Request::Recgonized(2));
        assert_eq!(rsp, 2);

        let rsp = router.call_ok(Request::Recgonized(2));
        assert_eq!(rsp, 4);
    }
}
