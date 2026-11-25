//! Input audit state shared by the evaluator and its host.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::HermeticSite;

thread_local! {
    static SCOPES: RefCell<Vec<(HermeticContext, Rc<Scope>)>> = const { RefCell::new(Vec::new()) };
}

enum Scope {
    Site(HermeticSite),
    FetchCache(PathBuf),
}

/// Source locations and the strict denial status of one configure.
#[derive(Clone)]
pub struct HermeticContext(Arc<State>);

struct State {
    default_site: HermeticSite,
    denied: AtomicBool,
}

/// Restores the previous source location when a command returns.
#[must_use]
pub struct HermeticSiteGuard {
    scope: Rc<Scope>,
}

impl HermeticContext {
    /// Set the location for input reads outside a command.
    pub fn new(default_site: HermeticSite) -> Self {
        Self(Arc::new(State {
            default_site,
            denied: AtomicBool::new(false),
        }))
    }

    /// Set the location for input reads on this thread until the guard is dropped.
    pub fn enter(&self, site: HermeticSite) -> HermeticSiteGuard {
        self.enter_scope(Scope::Site(site))
    }

    /// Return the active source location on this thread.
    pub fn site(&self) -> HermeticSite {
        self.find_scope(|scope| match scope {
            Scope::Site(site) => Some(site.clone()),
            Scope::FetchCache(_) => None,
        })
        .unwrap_or_else(|| self.0.default_site.clone())
    }

    /// Allow internal fetch operations to access a cache root on this thread.
    pub fn enter_fetch_cache(&self, path: PathBuf) -> HermeticSiteGuard {
        self.enter_scope(Scope::FetchCache(path))
    }

    /// Return the cache root of the active fetch operation on this thread.
    pub fn fetch_cache(&self) -> Option<PathBuf> {
        self.find_scope(|scope| match scope {
            Scope::FetchCache(path) => Some(path.clone()),
            Scope::Site(_) => None,
        })
    }

    fn enter_scope(&self, scope: Scope) -> HermeticSiteGuard {
        let scope = Rc::new(scope);
        SCOPES.with_borrow_mut(|scopes| scopes.push((self.clone(), Rc::clone(&scope))));
        HermeticSiteGuard { scope }
    }

    fn find_scope<T>(&self, get: impl Fn(&Scope) -> Option<T>) -> Option<T> {
        SCOPES.with_borrow(|scopes| {
            scopes
                .iter()
                .rev()
                .filter(|(context, _)| Arc::ptr_eq(&context.0, &self.0))
                .find_map(|(_, scope)| get(scope))
        })
    }

    /// Record a denied operation even when a command suppresses the host error.
    pub fn deny(&self) {
        self.0.denied.store(true, Ordering::Release);
    }

    /// Return whether the policy denied an operation in this session.
    pub fn denied(&self) -> bool {
        self.0.denied.load(Ordering::Acquire)
    }
}

impl Drop for HermeticSiteGuard {
    fn drop(&mut self) {
        SCOPES.with_borrow_mut(|scopes| {
            if let Some(index) = scopes
                .iter()
                .rposition(|(_, scope)| Rc::ptr_eq(scope, &self.scope))
            {
                scopes.remove(index);
            }
        });
    }
}
