//! Runtime-selected page drivers. A page is exclusively owned by the facade;
//! transports do not know about secrets, policies, scripts, or workflows.
use std::{future::Future, pin::Pin};
use anyhow::Result;
use serde_json::Value;
use super::{Point, Handle, cdp, bidi, camofox};

pub type DriverFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Borrowed input interfaces: a driver only exposes the operations it can do.
pub enum Pointer<'a> {
    Coordinates(&'a mut dyn CoordinatePointer),
    Targeted(&'a mut dyn TargetPointer),
}
pub enum Text<'a> {
    Focused(&'a mut dyn FocusedText),
    Targeted(&'a mut dyn TargetedText),
}
pub trait CoordinatePointer: Send {
    fn click_at(&mut self, point: Point) -> DriverFuture<'_, Result<InputReceipt, InputError>>;
}
pub trait TargetPointer: Send {
    fn click_selector<'a>(&'a mut self, selector: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>>;
}
pub trait FocusedText: Send {
    fn insert_text<'a>(&'a mut self, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>>;
}
pub trait TargetedText: Send {
    fn type_selector<'a>(&'a mut self, selector: &'a str, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>>;
}
pub trait EnterKey: Send {
    fn press_enter(&mut self) -> DriverFuture<'_, Result<InputReceipt, InputError>>;
}
pub trait Renewable: Send {
    fn renew(&mut self) -> DriverFuture<'_, Result<()>>;
}

/// A transport acknowledgment is not evidence that a website committed an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputReceipt;

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("input was not sent: {0}")]
    NotSent(#[source] anyhow::Error),
    #[error("input may have executed: {0}")]
    MayHaveExecuted(#[source] anyhow::Error),
}
impl InputError {
    pub fn conservative(error: anyhow::Error) -> Self {
        if error.is::<InputError>() { return error.downcast::<InputError>().unwrap(); }
        if error.is::<PageLost>() { Self::NotSent(error) } else { Self::MayHaveExecuted(error) }
    }
}
#[derive(Debug, thiserror::Error)]
#[error("page {0} was lost; recovery must explicitly renew or reacquire it")]
pub struct PageLost(pub String);

pub trait BrowserHost: Send + Sync {
    fn page(&self) -> DriverFuture<'_, Result<super::Browser>>;
    fn shutdown(&self) -> DriverFuture<'_, ()>;
}

struct CdpHost(std::sync::Arc<cdp::Host>);
impl BrowserHost for CdpHost {
    fn page(&self) -> DriverFuture<'_, Result<super::Browser>> {
        Box::pin(async { Ok(super::Browser::from_driver(cdp::Cdp::page(&self.0).await?)) })
    }
    fn shutdown(&self) -> DriverFuture<'_, ()> { Box::pin(async { self.0.shutdown().await }) }
}
struct BidiHost { shared: bidi::Shared, product: String }
impl BrowserHost for BidiHost {
    fn page(&self) -> DriverFuture<'_, Result<super::Browser>> {
        Box::pin(async { Ok(super::Browser::from_driver(bidi::Bidi::page(&self.shared, self.product.clone()).await?)) })
    }
    fn shutdown(&self) -> DriverFuture<'_, ()> { Box::pin(async { self.shared.shutdown().await }) }
}

pub trait PageDriver: Send {
    fn name(&self) -> &'static str;
    fn describe(&self) -> String;
    fn notes(&self) -> Vec<String> { Vec::new() }
    fn pointer(&mut self) -> Option<Pointer<'_>> { None }
    fn text(&mut self) -> Option<Text<'_>> { None }
    fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { None }
    fn renewable(&mut self) -> Option<&mut dyn Renewable> { None }
    fn preloads_bridge(&self) -> bool { true }
    fn host(&self) -> Handle { Handle::default() }
    fn page_id(&self) -> String;
    fn eval<'a>(&'a mut self, expression: &'a str) -> DriverFuture<'a, Result<Value>>;
    fn goto<'a>(&'a mut self, url: &'a str) -> DriverFuture<'a, Result<()>>;
    fn close(&mut self) -> DriverFuture<'_, ()>;
    fn follow_popup(&mut self) -> DriverFuture<'_, Result<bool>> { Box::pin(async { Ok(false) }) }
    fn wait_ready(&mut self) -> DriverFuture<'_, Result<()>> { Box::pin(async { Ok(()) }) }
}

fn acknowledged(result: Result<()>) -> Result<InputReceipt, InputError> {
    result.map(|()| InputReceipt).map_err(InputError::conservative)
}

impl PageDriver for cdp::Cdp {
    fn name(&self) -> &'static str { "cdp" }
    fn describe(&self) -> String { describe(&self.about, &self.host.product) }
    fn notes(&self) -> Vec<String> { self.notes.clone() }
    fn pointer(&mut self) -> Option<Pointer<'_>> { Some(Pointer::Coordinates(self)) }
    fn text(&mut self) -> Option<Text<'_>> { Some(Text::Focused(self)) }
    fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { Some(self) }
    fn renewable(&mut self) -> Option<&mut dyn Renewable> { Some(self) }
    fn host(&self) -> Handle { Handle::new(CdpHost(self.host.clone())) }
    fn page_id(&self) -> String { self.tab().target.clone() }
    fn eval<'a>(&'a mut self, expression: &'a str) -> DriverFuture<'a, Result<Value>> { Box::pin(async move { cdp::Cdp::eval(self, expression).await }) }
    fn goto<'a>(&'a mut self, url: &'a str) -> DriverFuture<'a, Result<()>> { Box::pin(async move { cdp::Cdp::goto(self, url).await }) }
    fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async move { self.shutdown().await }) }
    fn follow_popup(&mut self) -> DriverFuture<'_, Result<bool>> { Box::pin(async move { cdp::Cdp::follow_popup(self).await }) }
}
impl PageDriver for bidi::Bidi {
    fn name(&self) -> &'static str { "bidi" }
    fn describe(&self) -> String { describe(&self.about, &self.product) }
    fn notes(&self) -> Vec<String> { self.notes.clone() }
    fn pointer(&mut self) -> Option<Pointer<'_>> { Some(Pointer::Coordinates(self)) }
    fn text(&mut self) -> Option<Text<'_>> { Some(Text::Focused(self)) }
    fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { Some(self) }
    fn renewable(&mut self) -> Option<&mut dyn Renewable> { Some(self) }
    fn host(&self) -> Handle { Handle::new(BidiHost { shared: self.shared(), product: self.product.clone() }) }
    fn page_id(&self) -> String { self.ctx() }
    fn eval<'a>(&'a mut self, expression: &'a str) -> DriverFuture<'a, Result<Value>> { Box::pin(async move { bidi::Bidi::eval(self, expression).await }) }
    fn goto<'a>(&'a mut self, url: &'a str) -> DriverFuture<'a, Result<()>> { Box::pin(async move { bidi::Bidi::goto(self, url).await }) }
    fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async move { self.shutdown().await }) }
    fn follow_popup(&mut self) -> DriverFuture<'_, Result<bool>> { Box::pin(async move { bidi::Bidi::follow_popup(self).await }) }
}
impl PageDriver for camofox::Camofox {
    fn preloads_bridge(&self) -> bool { false }
    fn name(&self) -> &'static str { "camofox" }
    fn describe(&self) -> String { "camofox (Camoufox via camofox-browser)".into() }
    fn pointer(&mut self) -> Option<Pointer<'_>> { Some(Pointer::Targeted(self)) }
    fn text(&mut self) -> Option<Text<'_>> { Some(Text::Targeted(self)) }
    fn enter_key(&mut self) -> Option<&mut dyn EnterKey> { Some(self) }
    fn renewable(&mut self) -> Option<&mut dyn Renewable> { None }
    fn page_id(&self) -> String { self.tab.clone() }
    fn eval<'a>(&'a mut self, expression: &'a str) -> DriverFuture<'a, Result<Value>> { Box::pin(async move { camofox::Camofox::eval(self, expression).await }) }
    fn goto<'a>(&'a mut self, url: &'a str) -> DriverFuture<'a, Result<()>> { Box::pin(async move { camofox::Camofox::goto(self, url).await }) }
    fn close(&mut self) -> DriverFuture<'_, ()> { Box::pin(async move { camofox::Camofox::close(self).await }) }
    fn wait_ready(&mut self) -> DriverFuture<'_, Result<()>> { Box::pin(async move { camofox::Camofox::wait_ready(self).await }) }
}
fn describe(about: &str, product: &str) -> String { if product.is_empty() { about.into() } else { format!("{about} · {product}") } }

impl CoordinatePointer for cdp::Cdp {
    fn click_at(&mut self, point: Point) -> DriverFuture<'_, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(cdp::Cdp::click_at(self, point).await) }) }
}
impl FocusedText for cdp::Cdp {
    fn insert_text<'a>(&'a mut self, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(cdp::Cdp::insert_text(self, text).await) }) }
}
impl EnterKey for cdp::Cdp {
    fn press_enter(&mut self) -> DriverFuture<'_, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(cdp::Cdp::press_enter(self).await) }) }
}
impl Renewable for cdp::Cdp {
    fn renew(&mut self) -> DriverFuture<'_, Result<()>> { Box::pin(async move { cdp::Cdp::renew(self).await }) }
}
impl CoordinatePointer for bidi::Bidi {
    fn click_at(&mut self, point: Point) -> DriverFuture<'_, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(bidi::Bidi::click_at(self, point).await) }) }
}
impl FocusedText for bidi::Bidi {
    fn insert_text<'a>(&'a mut self, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(bidi::Bidi::insert_text(self, text).await) }) }
}
impl EnterKey for bidi::Bidi {
    fn press_enter(&mut self) -> DriverFuture<'_, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(bidi::Bidi::press_enter(self).await) }) }
}
impl Renewable for bidi::Bidi {
    fn renew(&mut self) -> DriverFuture<'_, Result<()>> { Box::pin(async move { bidi::Bidi::renew(self).await }) }
}
impl TargetPointer for camofox::Camofox {
    fn click_selector<'a>(&'a mut self, selector: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(camofox::Camofox::click_selector(self, selector).await) }) }
}
impl TargetedText for camofox::Camofox {
    fn type_selector<'a>(&'a mut self, selector: &'a str, text: &'a str) -> DriverFuture<'a, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(camofox::Camofox::type_selector(self, selector, text).await) }) }
}
impl EnterKey for camofox::Camofox {
    fn press_enter(&mut self) -> DriverFuture<'_, Result<InputReceipt, InputError>> { Box::pin(async move { acknowledged(self.press("Enter").await) }) }
}
