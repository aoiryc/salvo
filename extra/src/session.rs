/*!
# Salvo session support

Salvo session middleware is built on top of
[`async-session`](https://github.com/http-rs/async-session).

An example: [`session-login`](https://github.com/salvo-rs/salvo/tree/main/examples/session-login)

Sessions allows salvo to securely attach data to a browser session
allowing for retrieval and modification of this data within salvo
on subsequent visits. Session data is generally only retained for the
duration of a browser session.

## Stores

It is highly recommended that salvo applications use an
external-datastore-backed session storage. For a list of currently
available session stores, see [the documentation for
async-session](https://github.com/http-rs/async-session).

## Security

Although each session store may have different security implications,
the general approach of salvo's session system is as follows: On
each request, salvo checks the cookie configurable as `cookie_name`
on the handler.

### If no cookie is found:

A cryptographically random cookie value is generated. A cookie is set
on the outbound response and signed with an HKDF key derived from the
`secret` provided on creation of the SessionHandler.  The configurable
session store uses a SHA256 digest of the cookie value and stores the
session along with a potential expiry.

### If a cookie is found:

The hkdf derived signing key is used to verify the cookie value's
signature. If it verifies, it is then passed to the session store to
retrieve a Session. For most session stores, this will involve taking
a SHA256 digest of the cookie value and retrieving a serialized
Session from an external datastore based on that digest.

### Expiry

In addition to setting an expiry on the session cookie, salvo
sessions include the same expiry in their serialization format. If an
adversary were able to tamper with the expiry of a cookie, salvo
sessions would still check the expiry on the contained session before
using it

### If anything goes wrong with the above process

If there are any failures in the above session retrieval process, a
new empty session is generated for the request, which proceeds through
the application as normal.

## Stale/expired session cleanup

Any session store other than the cookie store will accumulate stale
sessions. Although the salvo session handler ensures that they
will not be used as valid sessions, For most session stores, it is the
salvo application's responsibility to call cleanup on the session
store if it requires it.
*/
pub use async_session::{CookieStore, MemoryStore, Session, SessionStore};

use std::fmt::{self, Formatter};
use std::time::Duration;

use async_session::base64;
use async_session::hmac::{Hmac, Mac, NewMac};
use async_session::sha2::Sha256;
use cookie::{Cookie, Key, SameSite};
use salvo_core::http::uri::Scheme;
use salvo_core::{async_trait, Depot, Error, FlowCtrl, Handler, Request, Response};

/// Key for store data in depot.
pub const SESSION_KEY: &str = "::salvo::extra::session";
const BASE64_DIGEST_LEN: usize = 44;

/// SessionDepotExt
pub trait SessionDepotExt {
    /// Set session
    fn set_session(&mut self, session: Session);
    /// Take session
    fn take_session(&mut self) -> Option<Session>;
    /// Get session reference
    fn session(&self) -> Option<&Session>;
    /// Get session mutable reference
    fn session_mut(&mut self) -> Option<&mut Session>;
}

impl SessionDepotExt for Depot {
    #[inline]
    fn set_session(&mut self, session: Session) {
        self.insert(SESSION_KEY, session);
    }
    #[inline]
    fn take_session(&mut self) -> Option<Session> {
        self.remove(SESSION_KEY)
    }
    #[inline]
    fn session(&self) -> Option<&Session> {
        self.get(SESSION_KEY)
    }
    #[inline]
    fn session_mut(&mut self) -> Option<&mut Session> {
        self.get_mut(SESSION_KEY)
    }
}

/// HandlerBuilder
pub struct HandlerBuilder<S> {
    store: S,
    cookie_path: String,
    cookie_name: String,
    cookie_domain: Option<String>,
    session_ttl: Option<Duration>,
    save_unchanged: bool,
    same_site_policy: SameSite,
    key: Key,
    fallback_keys: Vec<Key>,
}
impl<S: SessionStore> fmt::Debug for HandlerBuilder<S> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandlerBuilder")
            .field("store", &self.store)
            .field("cookie_path", &self.cookie_path)
            .field("cookie_name", &self.cookie_name)
            .field("cookie_domain", &self.cookie_domain)
            .field("session_ttl", &self.session_ttl)
            .field("same_site_policy", &self.same_site_policy)
            .field("key", &"..")
            .field("fallback_keys", &"..")
            .field("save_unchanged", &self.save_unchanged)
            .finish()
    }
}

impl<S> HandlerBuilder<S>
where
    S: SessionStore,
{
    /// Create new `HandlerBuilder`
    #[inline]
    pub fn new(store: S, secret: &[u8]) -> Self {
        Self {
            store,
            save_unchanged: true,
            cookie_path: "/".into(),
            cookie_name: "salvo.sid".into(),
            cookie_domain: None,
            same_site_policy: SameSite::Lax,
            session_ttl: Some(Duration::from_secs(24 * 60 * 60)),
            key: Key::from(secret),
            fallback_keys: vec![],
        }
    }

    /// Set a cookie path for this session middleware.
    ///
    /// The default for this value is "/".
    #[inline]
    pub fn cookie_path(mut self, cookie_path: impl AsRef<str>) -> Self {
        self.cookie_path = cookie_path.as_ref().to_owned();
        self
    }

    /// Set a session ttl. This will be used both for the cookie
    /// expiry and also for the session-internal expiry.
    ///
    /// The default for this value is one day. Set this to None to not
    /// set a cookie or session expiry. This is not recommended.
    #[inline]
    pub fn session_ttl(mut self, session_ttl: Option<Duration>) -> Self {
        self.session_ttl = session_ttl;
        self
    }

    /// Set the name of the cookie that the session is stored with or in.
    ///
    /// If you are running multiple tide applications on the same
    /// domain, you will need different values for each
    /// application. The default value is "tide.sid".
    #[inline]
    pub fn cookie_name(mut self, cookie_name: impl AsRef<str>) -> Self {
        self.cookie_name = cookie_name.as_ref().to_owned();
        self
    }

    /// Set the `save_unchanged` value.
    ///
    /// When `save_unchanged` is enabled, a session will cookie will always be set.
    ///
    /// With `save_unchanged` disabled, the session data must be modified
    /// from the `Default` value in order for it to save. If a session
    /// already exists and its data unmodified in the course of a
    /// request, the session will only be persisted if
    /// `save_unchanged` is enabled.
    #[inline]
    pub fn save_unchanged(mut self, value: bool) -> Self {
        self.save_unchanged = value;
        self
    }

    /// Set the same site policy for the session cookie. Defaults to
    /// SameSite::Lax. See [incrementally better
    /// cookies](https://tools.ietf.org/html/draft-west-cookie-incrementalism-01)
    /// for more information about this setting.
    #[inline]
    pub fn same_site_policy(mut self, policy: SameSite) -> Self {
        self.same_site_policy = policy;
        self
    }

    /// Set the domain of the cookie.
    #[inline]
    pub fn cookie_domain(mut self, cookie_domain: impl AsRef<str>) -> Self {
        self.cookie_domain = Some(cookie_domain.as_ref().to_owned());
        self
    }
    /// Set fallbacks.
    #[inline]
    pub fn fallback_keys(mut self, keys: Vec<impl Into<Key>>) -> Self {
        self.fallback_keys = keys.into_iter().map(|s| s.into()).collect();
        self
    }

    /// Add fallback secret.
    #[inline]
    pub fn add_fallback_key(mut self, key: impl Into<Key>) -> Self {
        self.fallback_keys.push(key.into());
        self
    }

    /// Build `SessionHandler`
    #[inline]
    pub fn build(self) -> Result<SessionHandler<S>, Error> {
        let Self {
            store,
            save_unchanged,
            cookie_path,
            cookie_name,
            cookie_domain,
            session_ttl,
            same_site_policy,
            key,
            fallback_keys,
        } = self;
        let hmac =
            Hmac::<Sha256>::new_from_slice(key.signing()).map_err(|_| Error::Other("invalid key length".into()))?;
        let fallback_hmacs = fallback_keys
            .iter()
            .map(|key| Hmac::<Sha256>::new_from_slice(key.signing()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::Other("invalid key length".into()))?;
        Ok(SessionHandler {
            store,
            save_unchanged,
            cookie_path,
            cookie_name,
            cookie_domain,
            session_ttl,
            same_site_policy,
            hmac,
            fallback_hmacs,
        })
    }
}

/// SessionHandler
pub struct SessionHandler<S> {
    store: S,
    cookie_path: String,
    cookie_name: String,
    cookie_domain: Option<String>,
    session_ttl: Option<Duration>,
    save_unchanged: bool,
    same_site_policy: SameSite,
    hmac: Hmac<Sha256>,
    fallback_hmacs: Vec<Hmac<Sha256>>,
}
impl<S: SessionStore> fmt::Debug for SessionHandler<S> {
    #[inline]
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionHandler")
            .field("store", &self.store)
            .field("cookie_path", &self.cookie_path)
            .field("cookie_name", &self.cookie_name)
            .field("cookie_domain", &self.cookie_domain)
            .field("session_ttl", &self.session_ttl)
            .field("same_site_policy", &self.same_site_policy)
            .field("key", &"..")
            .field("fallback_keys", &"..")
            .field("save_unchanged", &self.save_unchanged)
            .finish()
    }
}
#[async_trait]
impl<S> Handler for SessionHandler<S>
where
    S: SessionStore,
{
    async fn handle(&self, req: &mut Request, depot: &mut Depot, res: &mut Response, ctrl: &mut FlowCtrl) {
        let cookie = req.cookies().get(&self.cookie_name);
        let cookie_value = cookie.and_then(|cookie| self.verify_signature(cookie.value()).ok());

        let mut session = self.load_or_create(cookie_value).await;

        if let Some(ttl) = self.session_ttl {
            session.expire_in(ttl);
        }

        depot.set_session(session);

        ctrl.call_next(req, depot, res).await;
        if ctrl.is_ceased() {
            return;
        }

        let session = depot.take_session().expect("session should exist in depot");
        if session.is_destroyed() {
            if let Err(e) = self.store.destroy_session(session).await {
                tracing::error!(error = ?e, "unable to destroy session");
            }
            res.remove_cookie(self.cookie_name.clone());
        } else if self.save_unchanged || session.data_changed() {
            match self.store.store_session(session).await {
                Ok(cookie_value) => {
                    if let Some(cookie_value) = cookie_value {
                        let secure_cookie = req.uri().scheme() == Some(&Scheme::HTTPS);
                        let cookie = self.build_cookie(secure_cookie, cookie_value);
                        res.add_cookie(cookie);
                    }
                }
                Err(e) => {
                    tracing::error!(error = ?e, "store session error");
                }
            }
        }
    }
}

impl<S> SessionHandler<S>
where
    S: SessionStore,
{
    /// Create new `HandlerBuilder`
    pub fn builder(store: S, secret: &[u8]) -> HandlerBuilder<S> {
        HandlerBuilder::new(store, secret)
    }
    #[inline]
    async fn load_or_create(&self, cookie_value: Option<String>) -> Session {
        let session = match cookie_value {
            Some(cookie_value) => self.store.load_session(cookie_value).await.ok().flatten(),
            None => None,
        };

        session.and_then(|session| session.validate()).unwrap_or_default()
    }
    // the following is reused verbatim from
    // https://github.com/SergioBenitez/cookie-rs/blob/master/src/secure/signed.rs#L51-L66
    /// Given a signed value `str` where the signature is prepended to `value`,
    /// verifies the signed value and returns it. If there's a problem, returns
    /// an `Err` with a string describing the issue.
    #[inline]
    fn verify_signature(&self, cookie_value: &str) -> Result<String, Error> {
        if cookie_value.len() < BASE64_DIGEST_LEN {
            return Err(Error::Other("length of value is <= BASE64_DIGEST_LEN".into()));
        }

        // Split [MAC | original-value] into its two parts.
        let (digest_str, value) = cookie_value.split_at(BASE64_DIGEST_LEN);
        let digest = base64::decode(digest_str).map_err(|_| Error::Other("bad base64 digest".into()))?;

        // Perform the verification.
        let mut hmac = self.hmac.clone();
        hmac.update(value.as_bytes());
        if hmac.verify(&digest).is_ok() {
            return Ok(value.to_string());
        }
        for hmac in &self.fallback_hmacs {
            let mut hmac = hmac.clone();
            hmac.update(value.as_bytes());
            if hmac.verify(&digest).is_ok() {
                return Ok(value.to_string());
            }
        }
        Err(Error::Other("value did not verify".into()))
    }
    #[inline]
    fn build_cookie(&self, secure: bool, cookie_value: String) -> Cookie<'static> {
        let mut cookie = Cookie::build(self.cookie_name.clone(), cookie_value)
            .http_only(true)
            .same_site(self.same_site_policy)
            .secure(secure)
            .path(self.cookie_path.clone())
            .finish();

        if let Some(ttl) = self.session_ttl {
            cookie.set_expires(Some((std::time::SystemTime::now() + ttl).into()));
        }

        if let Some(cookie_domain) = self.cookie_domain.clone() {
            cookie.set_domain(cookie_domain)
        }

        self.sign_cookie(&mut cookie);

        cookie
    }
    // The following is reused verbatim from
    // https://github.com/SergioBenitez/cookie-rs/blob/master/src/secure/signed.rs#L37-46
    /// signs the cookie's value providing integrity and authenticity.
    #[inline]
    fn sign_cookie(&self, cookie: &mut Cookie<'_>) {
        // Compute HMAC-SHA256 of the cookie's value.
        let mut mac = self.hmac.clone();
        mac.update(cookie.value().as_bytes());

        // Cookie's new value is [MAC | original-value].
        let mut new_value = base64::encode(&mac.finalize().into_bytes());
        new_value.push_str(cookie.value());
        cookie.set_value(new_value);
    }
}

#[cfg(test)]
mod tests {
    use salvo_core::http::Method;
    use salvo_core::http::header::*;
    use salvo_core::prelude::*;
    use salvo_core::test::{ResponseExt, TestClient};
    use salvo_core::writer::Redirect;

    use super::*;

    #[test]
    fn test_session_data() {
        let handler = SessionHandler::builder(
            async_session::CookieStore,
            b"secretabsecretabsecretabsecretabsecretabsecretabsecretabsecretab",
        )
        .cookie_domain("test.domain")
        .cookie_name("test_cookie")
        .cookie_path("/abc")
        .same_site_policy(SameSite::Strict)
        .session_ttl(Some(Duration::from_secs(30)))
        .build()
        .unwrap();
        assert_eq!(handler.cookie_domain, Some("test.domain".into()));
        assert_eq!(handler.cookie_name, "test_cookie");
        assert_eq!(handler.cookie_path, "/abc");
        assert_eq!(handler.same_site_policy, SameSite::Strict);
        assert_eq!(handler.session_ttl, Some(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn test_session_login() {
        #[handler]
        pub async fn login(req: &mut Request, depot: &mut Depot, res: &mut Response) {
            if req.method() == Method::POST {
                let mut session = Session::new();
                session
                    .insert("username", req.form::<String>("username").await.unwrap())
                    .unwrap();
                depot.set_session(session);
                res.render(Redirect::other("/").unwrap());
            } else {
                res.render(Text::Html("login page"));
            }
        }

        #[handler]
        pub async fn logout(depot: &mut Depot, res: &mut Response) {
            if let Some(session) = depot.session_mut() {
                session.remove("username");
            }
            res.render(Redirect::other("/").unwrap());
        }

        #[handler]
        pub async fn home(depot: &mut Depot, res: &mut Response) {
            let mut content = r#"home"#.into();
            if let Some(session) = depot.session_mut() {
                if let Some(username) = session.get::<String>("username") {
                    content = username;
                }
            }
            res.render(Text::Html(content));
        }
        
        let session_handler = SessionHandler::builder(
            MemoryStore::new(),
            b"secretabsecretabsecretabsecretabsecretabsecretabsecretabsecretab",
        )
        .build()
        .unwrap();
        let router = Router::new()
            .hoop(session_handler)
            .get(home)
            .push(Router::with_path("login").get(login).post(login))
            .push(Router::with_path("logout").get(logout));
        let service = Service::new(router);

        let respone = TestClient::post("http://127.0.0.1:7878/login").raw_form("username=salvo").send(&service).await;
        assert_eq!(respone.status_code(), Some(StatusCode::SEE_OTHER));
        let cookie = respone.headers().get(SET_COOKIE).unwrap();
        
        let mut respone = TestClient::get("http://127.0.0.1:7878/").add_header(COOKIE, cookie, true).send(&service).await;
        assert_eq!(respone.take_string().await.unwrap(), "salvo");
        
        let respone = TestClient::get("http://127.0.0.1:7878/logout").send(&service).await;
        assert_eq!(respone.status_code(), Some(StatusCode::SEE_OTHER));

        let mut respone = TestClient::get("http://127.0.0.1:7878/").send(&service).await;
        assert_eq!(respone.take_string().await.unwrap(), "home");
    }
}
