pub mod bidi;
pub mod camofox;
pub mod cdp;
pub mod discover;
pub mod driver;

use driver::{PageDriver, Pointer, Text, InputError};
use tokio::sync::Mutex;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::config::{ExecMode, Knobs};

pub const SNAPSHOT_JS: &str = include_str!("../js/snapshot.js");

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SettleInfo {
    pub ms: f64,
    pub navigations: u32,
    pub timed_out: bool,
}

/// Shared policy/runtime facade. Each compound operation owns the driver lock
/// until its final protocol acknowledgment, including focus/type/commit.
pub struct Browser {
    runtime: Mutex<Box<dyn PageDriver>>,
    name: &'static str,
    description: String,
    notes: Vec<String>,
    host: Handle,
}

/// Optional ownership of a browser capable of creating independent pages.
/// Concrete transports implement the capability; external drivers can provide
/// their own host without changing a backend-selection enum.
#[derive(Clone, Default)]
pub struct Handle(Option<std::sync::Arc<dyn driver::BrowserHost>>);
impl Handle {
    pub fn new(host: impl driver::BrowserHost + 'static) -> Self { Self(Some(std::sync::Arc::new(host))) }
    pub fn capability(&self) -> Option<&dyn driver::BrowserHost> { self.0.as_deref() }
    pub async fn page(&self) -> Result<Browser> {
        self.capability().ok_or_else(|| anyhow::anyhow!("this backend has one page"))?.page().await
    }
    pub fn multi(&self) -> bool { self.0.is_some() }
    pub async fn shutdown(&self) { if let Some(host) = self.capability() { host.shutdown().await; } }
}

pub(crate) fn nonce() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0)
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

impl Browser {
    pub async fn launch(k: &Knobs) -> Result<Self> {
        Ok(match k.backend.as_str() {
            "cdp" | "chrome" => {
                let dialogs = if k.dialog == "dismiss" { cdp::Dialogs::Dismiss } else { cdp::Dialogs::Accept };
                if !k.connect.is_empty() {
                    return Ok(Browser::from_driver(cdp::Cdp::attach(&k.connect, dialogs).await?));
                }
                let (found, mut notes) = discover::pick(&k.browser)?;
                let mut profile = k.profile_dir(&found.id);
                if found.engine == discover::Engine::Firefox {
                    if let Some(p) = profile.clone() {
                        match bidi::running(&p) {
                            // Its Firefox is still open: re-use it (window, tabs, logins).
                            Some(bidi::Running::Bidi(port)) => match bidi::Bidi::attach(port, dialogs).await {
                                Ok(mut b) => {
                                    b.about = format!("{} ({}) · profile {} · re-used the open window", found.name, found.source, p.display());
                                    notes.push(format!("re-used the Firefox already open on profile {}", p.display()));
                                    b.notes = notes;
                                    return Ok(Browser::from_driver(b));
                                }
                                Err(e) => {
                                    notes.push(format!(
                                        "profile {} is open in a Firefox another program is driving ({}); using a temporary profile. Close that one, or share it: every fab -s NAME command drives the same session",
                                        p.display(),
                                        crate::snapshot::truncate(&format!("{e:#}"), 80)
                                    ));
                                    profile = None;
                                }
                            },
                            Some(bidi::Running::Plain) => {
                                notes.push(format!("profile {} is open in a Firefox fab can't control; using a temporary profile (quit that Firefox to use the profile)", p.display()));
                                profile = None;
                            }
                            None => {}
                        }
                    }
                } else if let Some(p) = profile.as_ref().filter(|p| cdp::profile_in_use(p)) {
                    notes.push(format!("profile {} is open in another browser; using a temporary profile", p.display()));
                    profile = None;
                }
                let about = format!(
                    "{} ({}) · {}",
                    found.name,
                    found.source,
                    match &profile {
                        Some(p) => format!("profile {}", p.display()),
                        None => "temporary profile".into(),
                    }
                );
                if found.engine == discover::Engine::Firefox {
                    let launch = cdp::Launch { bin: found.path, headless: !k.headful, profile, extra: vec![] };
                    let mut b = bidi::Bidi::start(&launch, dialogs).await?;
                    b.about = about;
                    b.notes = notes;
                    return Ok(Browser::from_driver(b));
                }
                let extra = k.window.as_deref().map(crate::config::window_args).unwrap_or_default();
                let launch = cdp::Launch { bin: found.path, headless: !k.headful, profile, extra };
                let mut c = cdp::Cdp::start(&launch, dialogs).await?;
                c.about = about;
                c.notes = notes;
                Browser::from_driver(c)
            }
            "camofox" => Browser::from_driver(camofox::Camofox::open(&k.camofox_url).await?),
            other => bail!("unknown backend {other}"),
        })
    }

    /// Extension point for third-party drivers; selection happens only at startup.
    pub fn from_driver(driver: impl PageDriver + 'static) -> Self {
        Self::from_boxed_driver(Box::new(driver))
    }

    pub fn from_boxed_driver(driver: Box<dyn PageDriver>) -> Self {
        Self {
            name: driver.name(), description: driver.describe(), notes: driver.notes(),
            host: driver.host(), runtime: Mutex::new(driver),
        }
    }
    pub fn name(&self) -> &'static str { self.name }
    pub fn describe(&self) -> String { self.description.clone() }
    pub fn notes(&self) -> Vec<String> { self.notes.clone() }
    pub fn handle(&self) -> Handle { self.host.clone() }
    /// Transport page identity for diagnostics; never an execution capability.
    pub async fn page_id(&self) -> String { self.runtime.lock().await.page_id() }
    pub async fn eval(&self, expression: &str) -> Result<Value> {
        evaluate(&mut **self.runtime.lock().await, expression).await
    }
    pub async fn close(&self) { self.runtime.lock().await.close().await; }
    /// Probe only: a lost page is never replaced by a read or health check.
    pub async fn healthy(&self, within: Duration) -> bool {
        matches!(tokio::time::timeout(within, self.eval("1")).await, Ok(Ok(v)) if v == 1)
    }
    pub async fn renew(&self) -> Result<()> {
        let mut driver = self.runtime.lock().await;
        let renew = driver.renewable().ok_or_else(|| anyhow::anyhow!("page renewal is unavailable"))?;
        renew.renew().await
    }

    pub async fn goto(&self, url: &str, k: &Knobs) -> Result<SettleInfo> {
        {
            let mut driver = self.runtime.lock().await;
            driver.goto(url).await?;
            driver.wait_ready().await.ok();
        }
        let doc = self.doc_id().await.unwrap_or_default();
        self.settle(k, &doc).await
    }
    pub async fn doc_id(&self) -> Result<String> {
        Ok(self.eval("__ub.docId").await?.as_str().unwrap_or_default().to_string())
    }
    pub async fn click(&self, i: usize, mode: ExecMode) -> Result<()> {
        let mut driver = self.runtime.lock().await;
        let d = &mut **driver;
        if matches!(mode, ExecMode::Js) {
            evaluate(d, &format!("__ub.click({i})")).await.map_err(InputError::conservative)?;
        } else {
            let coordinate = match d.pointer() {
                Some(Pointer::Coordinates(_)) => Some(true),
                Some(Pointer::Targeted(_)) => Some(false),
                None => None,
            };
            match coordinate {
                Some(true) => {
                    let point: Point = serde_json::from_value(evaluate(d, &format!("__ub.point({i})")).await.map_err(InputError::conservative)?)
                        .map_err(|e| InputError::NotSent(e.into()))?;
                    match d.pointer() {
                        Some(Pointer::Coordinates(pointer)) => { pointer.click_at(point).await?; }
                        _ => return Err(InputError::NotSent(anyhow::anyhow!("native pointer capability changed")).into()),
                    }
                }
                Some(false) => {
                    let selector = selector(d, i).await?;
                    match d.pointer() {
                        Some(Pointer::Targeted(pointer)) => { pointer.click_selector(&selector).await?; }
                        _ => return Err(InputError::NotSent(anyhow::anyhow!("native pointer capability changed")).into()),
                    }
                }
                None => return Err(InputError::NotSent(anyhow::anyhow!("native pointer input is unavailable")).into()),
            }
        }
        Ok(())
    }
    pub async fn fill(&self, i: usize, text: &str, mode: ExecMode) -> Result<()> {
        let mut driver = self.runtime.lock().await;
        let d = &mut **driver;
        if crate::secrets::has_placeholder(text) {
            let target: crate::secrets::Target = serde_json::from_value(evaluate(d, &format!("__ub.target({i})")).await?)?;
            let value = crate::secrets::vault().substitute_for_input(text, &target).await?;
            if value.concealed { evaluate(d, &format!("__ub.markSecret({i})")).await?; }
            fill_native(d, i, &value.value).await?;
        } else if matches!(mode, ExecMode::Js) {
            evaluate(d, &format!("__ub.fill({i},{})", js_str(text))).await.map_err(InputError::conservative)?;
        } else {
            fill_native(d, i, text).await?;
        }
        Ok(())
    }
    pub async fn select(&self, i: usize, label: &str) -> Result<()> {
        let mut driver = self.runtime.lock().await;
        let d = &mut **driver;
        let label = if crate::secrets::has_placeholder(label) {
            let mut target: crate::secrets::Target = serde_json::from_value(evaluate(d, &format!("__ub.target({i})")).await?)?;
            target.editable = true;
            let value = crate::secrets::vault().substitute(label, &target).await?;
            if value.concealed { bail!("a concealed value can't be chosen from a list"); }
            value.value.to_string()
        } else { label.to_string() };
        evaluate(d, &format!("__ub.select({i},{})", js_str(&label))).await.map_err(InputError::conservative)?;
        Ok(())
    }
    pub async fn enter(&self, i: Option<usize>, mode: ExecMode) -> Result<()> {
        let mut driver = self.runtime.lock().await;
        let d = &mut **driver;
        if matches!(mode, ExecMode::Js) {
            let arg = i.map(|i| i.to_string()).unwrap_or_else(|| "null".into());
            evaluate(d, &format!("__ub.enter({arg})")).await.map_err(InputError::conservative)?;
        } else {
            if d.enter_key().is_none() { return Err(InputError::NotSent(anyhow::anyhow!("native enter is unavailable")).into()); }
            if let Some(i) = i {
                evaluate(d, &format!("__ub.el({i}).focus()")).await.map_err(InputError::conservative)?;
                // Focus may run page handlers before the key is dispatched.
                d.enter_key().ok_or_else(|| InputError::NotSent(anyhow::anyhow!("native enter capability changed")))?
                    .press_enter().await.map_err(|e| InputError::MayHaveExecuted(e.into()))?;
            } else {
                d.enter_key().ok_or_else(|| InputError::NotSent(anyhow::anyhow!("native enter capability changed")))?
                    .press_enter().await?;
            }
        }
        Ok(())
    }
    async fn follow_popup(&self) -> bool { self.runtime.lock().await.follow_popup().await.unwrap_or(false) }

    /// Waits for the page to go quiet after an action. If the action started a
    /// navigation, waits for the new document and settles that instead; if it
    /// opened a new tab, continues there.
    pub async fn settle(&self, k: &Knobs, doc: &str) -> Result<SettleInfo> {
        let t0 = Instant::now();
        let mut info = SettleInfo::default();
        let mut doc = doc.to_string();
        loop {
            let expr = format!(
                "__ub.settle({},{},{}).then(r => (r.doc = __ub.docId, r))",
                k.quiet_ms, k.settle_cap_ms, k.timer_max_ms
            );
            let nav = match self.eval(&expr).await {
                Ok(v) => {
                    if let Some(d) = v.get("doc").and_then(Value::as_str) {
                        doc = d.to_string();
                    }
                    info.timed_out |= v.get("timeout").is_some();
                    v.get("nav").is_some()
                }
                // The execution context died mid-settle: a navigation committed.
                Err(_) => true,
            };
            if !nav || info.navigations >= 4 {
                if info.navigations < 4 && self.follow_popup().await {
                    info.navigations += 1;
                    doc = self.doc_id().await.unwrap_or_default();
                    continue;
                }
                break;
            }
            info.navigations += 1;
            doc = self.wait_new_doc(&doc).await?;
            self.runtime.lock().await.wait_ready().await.ok();
        }
        info.ms = t0.elapsed().as_secs_f64() * 1e3;
        Ok(info)
    }

    async fn wait_new_doc(&self, old: &str) -> Result<String> {
        let t0 = Instant::now();
        loop {
            if let Ok(v) = self.eval("[__ub.docId, document.readyState]").await {
                let d = v[0].as_str().unwrap_or_default();
                if d != old && v[1] != "loading" {
                    return Ok(d.to_string());
                }
            }
            if t0.elapsed() > Duration::from_secs(10) {
                // Navigation never committed (download, 204, cancelled): carry on.
                let _ = self.eval("(__ub.navigating = false, __ub.docId)").await;
                return Ok(old.to_string());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// One bridge guard used by every backend. Drivers supporting document preload
/// install it there; other transports inject it lazily without leaking secrets.
async fn evaluate(driver: &mut dyn PageDriver, expression: &str) -> Result<Value> {
    if driver.preloads_bridge() {
        driver.eval(expression).await
    } else {
        driver.eval(&format!("(function(){{{SNAPSHOT_JS}\nreturn ({expression});}})()")).await
    }
}

async fn selector(driver: &mut dyn PageDriver, i: usize) -> Result<String> {
    let value = evaluate(driver, &format!("__ub.selector({i})")).await.map_err(InputError::conservative)?;
    value.as_str().filter(|s| !s.is_empty()).map(str::to_string)
        .ok_or_else(|| InputError::NotSent(anyhow::anyhow!("target has no selector")).into())
}

async fn fill_native(driver: &mut dyn PageDriver, i: usize, text: &str) -> Result<()> {
    let focused = match driver.text() {
        Some(Text::Focused(_)) => Some(true),
        Some(Text::Targeted(_)) => Some(false),
        None => None,
    };
    match focused {
        Some(true) => {
            // Focus/clear can trigger page handlers. Every subsequent failure
            // therefore remains uncertain even if the next packet was not sent.
            evaluate(driver, &format!("__ub.focusClear({i})")).await.map_err(InputError::conservative)?;
            driver.text().and_then(|text| match text { Text::Focused(text) => Some(text), _ => None })
                .ok_or_else(|| InputError::MayHaveExecuted(anyhow::anyhow!("native text capability changed after focus")))?
                .insert_text(text).await.map_err(|e| InputError::MayHaveExecuted(e.into()))?;
            evaluate(driver, &format!("__ub.commit({i})")).await.map_err(|e| InputError::MayHaveExecuted(e))?;
        }
        Some(false) => {
            let selector = selector(driver, i).await?;
            driver.text().and_then(|text| match text { Text::Targeted(text) => Some(text), _ => None })
                .ok_or_else(|| InputError::NotSent(anyhow::anyhow!("native text capability changed")))?
                .type_selector(&selector, text).await?;
        }
        None => return Err(InputError::NotSent(anyhow::anyhow!("native text input is unavailable")).into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use driver::{DriverFuture, InputReceipt, PageLost, CoordinatePointer, TargetPointer, FocusedText, TargetedText, EnterKey, Renewable};
    #[derive(Clone, Copy)] enum NativePointer { Coordinates, Targeted }
    #[derive(Clone, Copy)] enum NativeText { Focused, Targeted }
    use std::sync::{Arc, Mutex as StdMutex};

    struct Fake {
        events: Arc<StdMutex<Vec<String>>>,
        pointer: NativePointer,
        text: NativeText,
        lost: bool,
    }

    impl Fake {
        fn new(pointer: NativePointer, text: NativeText) -> (Browser, Arc<StdMutex<Vec<String>>>) {
            let events = Arc::new(StdMutex::new(Vec::new()));
            (Browser::from_driver(Self { events: events.clone(), pointer, text, lost: false }), events)
        }
        fn record(&self, event: String) { self.events.lock().unwrap().push(event); }
    }

    impl PageDriver for Fake {
        fn name(&self) -> &'static str { "fake" }
        fn describe(&self) -> String { "fake page".into() }
        fn pointer(&mut self) -> Option<Pointer<'_>> { Some(match self.pointer { NativePointer::Coordinates => Pointer::Coordinates(self), NativePointer::Targeted => Pointer::Targeted(self) }) }
        fn text(&mut self) -> Option<Text<'_>> { Some(match self.text { NativeText::Focused => Text::Focused(self), NativeText::Targeted => Text::Targeted(self) }) }
        fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { Some(self) }
        fn renewable(&mut self) -> Option<&mut dyn Renewable> { Some(self) }
        fn page_id(&self) -> String { "fake-page".into() }
        fn eval<'a>(&'a mut self, expression: &'a str) -> DriverFuture<'a, Result<Value>> {
            Box::pin(async move {
                if self.lost { return Err(PageLost("fake-page".into()).into()); }
                self.record(format!("eval:{expression}"));
                if expression.starts_with("__ub.point(") { Ok(serde_json::json!({"x": 3.0, "y": 4.0})) }
                else if expression.starts_with("__ub.selector(") { Ok(Value::String("#target".into())) }
                else { Ok(Value::from(1)) }
            })
        }
        fn goto<'a>(&'a mut self, url: &'a str) -> DriverFuture<'a, Result<()>> {
            Box::pin(async move { self.record(format!("goto:{url}")); Ok(()) })
        }
        fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async move { self.record("close".into()); }) }
    }
    impl Renewable for Fake {
        fn renew(&mut self) -> DriverFuture<'_, Result<()>> {
            Box::pin(async move { self.lost = false; self.record("renew".into()); Ok(()) })
        }
    }
    impl CoordinatePointer for Fake {
        fn click_at(&mut self, point: Point) -> DriverFuture<'_, Result<InputReceipt, InputError>> {
            Box::pin(async move { self.record(format!("click:{},{}", point.x, point.y)); Ok(InputReceipt) })
        }
    }
    impl TargetPointer for Fake {
        fn click_selector<'a>(&'a mut self, selector: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> {
            Box::pin(async move { self.record(format!("click:{selector}")); Ok(InputReceipt) })
        }
    }
    impl FocusedText for Fake {
        fn insert_text<'a>(&'a mut self, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> {
            Box::pin(async move { self.record(format!("text:{text}")); Ok(InputReceipt) })
        }
    }
    impl TargetedText for Fake {
        fn type_selector<'a>(&'a mut self, selector: &'a str, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> {
            Box::pin(async move { self.record(format!("type:{selector}:{text}")); Ok(InputReceipt) })
        }
    }
    impl EnterKey for Fake {
        fn press_enter(&mut self) -> DriverFuture<'_, Result<InputReceipt, InputError>> {
            Box::pin(async move { self.record("enter".into()); Ok(InputReceipt) })
        }
    }

    struct EvalOnly;
    impl PageDriver for EvalOnly {
        fn name(&self) -> &'static str { "eval-only" }
        fn describe(&self) -> String { "eval-only page".into() }
        fn pointer(&mut self) -> Option<Pointer<'_>> { None }
        fn text(&mut self) -> Option<Text<'_>> { None }
        fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { None }
        fn renewable(&mut self) -> Option<&mut dyn Renewable> { None }
        fn page_id(&self) -> String { "eval-only".into() }
        fn eval<'a>(&'a mut self, _expression: &'a str) -> DriverFuture<'a, Result<Value>> {
            Box::pin(async { Ok(Value::Null) })
        }
        fn goto<'a>(&'a mut self, _url: &'a str) -> DriverFuture<'a, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async {}) }
    }

    #[tokio::test]
    async fn absent_capabilities_cannot_dispatch_native_input() {
        let browser = Browser::from_driver(EvalOnly);
        for error in [
            browser.click(0, ExecMode::Trusted).await.unwrap_err(),
            browser.fill(0, "text", ExecMode::Trusted).await.unwrap_err(),
            browser.enter(None, ExecMode::Trusted).await.unwrap_err(),
        ] {
            assert!(matches!(error.downcast_ref::<InputError>(), Some(InputError::NotSent(_))));
        }
        assert!(browser.renew().await.is_err());
    }


    #[tokio::test]
    async fn independent_pointer_and_text_capabilities_route_operations() {
        let (browser, events) = Fake::new(NativePointer::Coordinates, NativeText::Targeted);
        browser.click(2, ExecMode::Trusted).await.unwrap();
        browser.fill(2, "hello", ExecMode::Trusted).await.unwrap();
        browser.enter(Some(2), ExecMode::Trusted).await.unwrap();
        assert_eq!(*events.lock().unwrap(), [
            "eval:__ub.point(2)", "click:3,4", "eval:__ub.selector(2)",
            "type:#target:hello", "eval:__ub.el(2).focus()", "enter",
        ]);
    }

    #[tokio::test]
    async fn focused_text_keeps_focus_type_commit_on_one_driver() {
        let (browser, events) = Fake::new(NativePointer::Targeted, NativeText::Focused);
        browser.click(1, ExecMode::Trusted).await.unwrap();
        browser.fill(1, "hello", ExecMode::Trusted).await.unwrap();
        assert_eq!(*events.lock().unwrap(), [
            "eval:__ub.selector(1)", "click:#target", "eval:__ub.focusClear(1)",
            "text:hello", "eval:__ub.commit(1)",
        ]);
    }

    #[tokio::test]
    async fn page_loss_needs_explicit_renewal() {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let browser = Browser::from_driver(Fake {
            events: events.clone(), pointer: NativePointer::Coordinates, text: NativeText::Focused, lost: true,
        });
        assert!(!browser.healthy(Duration::from_millis(100)).await);
        let error = browser.click(0, ExecMode::Js).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<InputError>(), Some(InputError::NotSent(_))));
        assert!(events.lock().unwrap().is_empty());
        browser.renew().await.unwrap();
        assert!(browser.healthy(Duration::from_millis(100)).await);
        assert_eq!(*events.lock().unwrap(), ["renew", "eval:1"]);
    }

    #[test]
    fn ambiguous_transport_errors_are_conservative() {
        assert!(matches!(InputError::conservative(anyhow::anyhow!("connection dropped")), InputError::MayHaveExecuted(_)));
        assert!(matches!(InputError::conservative(PageLost("lost".into()).into()), InputError::NotSent(_)));
    }
}
