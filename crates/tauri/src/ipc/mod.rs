// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Types and functions related to Inter Procedure Call(IPC).
//!
//! This module includes utilities to send messages to the JS layer of the webview.

use std::{
  future::Future,
  sync::{Arc, Mutex},
};

use http::HeaderMap;
use serde::{
  de::{DeserializeOwned, IntoDeserializer},
  ser::{
    self, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant,
  },
  Deserialize, Serialize,
};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
pub use serialize_to_javascript::Options as SerializeOptions;
use tauri_macros::default_runtime;
use tauri_utils::acl::resolved::ResolvedCommand;

use crate::{webview::Webview, Runtime, StateManager};

mod authority;
#[cfg(feature = "dynamic-acl")]
mod capability_builder;
pub(crate) mod channel;
mod command;
pub(crate) mod format_callback;
pub(crate) mod protocol;

pub use authority::{
  CommandScope, GlobalScope, Origin, RuntimeAuthority, ScopeObject, ScopeObjectMatch, ScopeValue,
};
#[cfg(feature = "dynamic-acl")]
pub use capability_builder::{CapabilityBuilder, RuntimeCapability};
pub use channel::{Channel, JavaScriptChannelId};
pub use command::{private, CommandArg, CommandItem};

/// A closure that is run every time Tauri receives a message it doesn't explicitly handle.
pub type InvokeHandler<R> = dyn Fn(Invoke<R>) -> bool + Send + Sync + 'static;

/// A closure that is responsible for respond a JS message.
pub type InvokeResponder<R> =
  dyn Fn(&Webview<R>, &str, &InvokeResponse, CallbackFn, CallbackFn) + Send + Sync + 'static;
/// Similar to [`InvokeResponder`] but taking owned arguments.
pub type OwnedInvokeResponder<R> =
  dyn FnOnce(Webview<R>, String, InvokeResponse, CallbackFn, CallbackFn) + Send + 'static;

/// Possible values of an IPC payload.
///
/// ### Android
/// On Android, [InvokeBody::Raw] is not supported. The enum will always contain [InvokeBody::Json].
/// When targeting Android Devices, consider passing raw bytes as a base64 [[std::string::String]], which is still more efficient than passing them as a number array in [InvokeBody::Json]
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum InvokeBody {
  /// Json payload.
  Json(JsonValue),
  /// Bytes payload.
  Raw(Vec<u8>),
}

impl Default for InvokeBody {
  fn default() -> Self {
    Self::Json(Default::default())
  }
}

impl From<JsonValue> for InvokeBody {
  fn from(value: JsonValue) -> Self {
    Self::Json(value)
  }
}

impl From<Vec<u8>> for InvokeBody {
  fn from(value: Vec<u8>) -> Self {
    Self::Raw(value)
  }
}

impl InvokeBody {
  #[cfg(mobile)]
  pub(crate) fn into_json(self) -> JsonValue {
    match self {
      Self::Json(v) => v,
      Self::Raw(v) => {
        JsonValue::Array(v.into_iter().map(|n| JsonValue::Number(n.into())).collect())
      }
    }
  }
}

/// Possible values of an IPC response.
#[derive(Debug, Clone)]
#[cfg_attr(test, derive(PartialEq))]
pub enum InvokeResponseBody {
  /// Json payload.
  Json(String),
  /// Bytes payload.
  Raw(Vec<u8>),
}

impl From<String> for InvokeResponseBody {
  fn from(value: String) -> Self {
    Self::Json(value)
  }
}

impl From<Vec<u8>> for InvokeResponseBody {
  fn from(value: Vec<u8>) -> Self {
    Self::Raw(value)
  }
}

impl From<InvokeBody> for InvokeResponseBody {
  fn from(value: InvokeBody) -> Self {
    match value {
      InvokeBody::Json(v) => Self::Json(serde_json::to_string(&v).unwrap()),
      InvokeBody::Raw(v) => Self::Raw(v),
    }
  }
}

impl IpcResponse for InvokeResponseBody {
  fn body(self) -> crate::Result<InvokeResponseBody> {
    Ok(self)
  }
}

impl InvokeResponseBody {
  /// Attempts to deserialize the response.
  pub fn deserialize<T: DeserializeOwned>(self) -> serde_json::Result<T> {
    match self {
      Self::Json(v) => serde_json::from_str(&v),
      Self::Raw(v) => T::deserialize(v.into_deserializer()),
    }
  }
}

/// The IPC request.
///
/// Includes the `body` and `headers` parameters of a Tauri command invocation.
/// This allows commands to accept raw bytes - on all platforms except Android.
#[derive(Debug)]
pub struct Request<'a> {
  body: &'a InvokeBody,
  headers: &'a HeaderMap,
}

impl Request<'_> {
  /// The request body.
  pub fn body(&self) -> &InvokeBody {
    self.body
  }

  /// Thr request headers.
  pub fn headers(&self) -> &HeaderMap {
    self.headers
  }
}

impl<'a, R: Runtime> CommandArg<'a, R> for Request<'a> {
  /// Returns the invoke [`Request`].
  fn from_command(command: CommandItem<'a, R>) -> Result<Self, InvokeError> {
    Ok(Self {
      body: command.message.payload(),
      headers: command.message.headers(),
    })
  }
}

/// Marks a type as a response to an IPC call.
pub trait IpcResponse {
  /// Resolve the IPC response body.
  fn body(self) -> crate::Result<InvokeResponseBody>;
}

const JS_MAX_SAFE_INT: i128 = 9_007_199_254_740_991;

struct IpcResponseSerializer;

fn bigint_value(value: impl ToString) -> JsonValue {
  let mut object = JsonMap::new();
  object.insert("$bigint".into(), JsonValue::String(value.to_string()));
  JsonValue::Object(object)
}

fn is_unsafe_js_integer(value: i128) -> bool {
  !(-JS_MAX_SAFE_INT..=JS_MAX_SAFE_INT).contains(&value)
}

fn non_finite_bigint_value(kind: &str, negative: bool) -> JsonValue {
  let mut object = JsonMap::new();
  object.insert("$bigint".into(), JsonValue::Bool(true));
  object.insert(kind.into(), JsonValue::Bool(true));
  if negative {
    object.insert("$bigint_negative".into(), JsonValue::Bool(true));
    if kind == "$bigint_infinity" {
      object.insert("$bigint_negative_infinity".into(), JsonValue::Bool(true));
    }
  }
  JsonValue::Object(object)
}

fn finite_f64_value(value: f64) -> Result<JsonValue, serde_json::Error> {
  JsonNumber::from_f64(value)
    .map(JsonValue::Number)
    .ok_or_else(|| ser::Error::custom("invalid floating point value"))
}

impl ser::Serializer for IpcResponseSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;
  type SerializeSeq = JsonVecSerializer;
  type SerializeTuple = JsonVecSerializer;
  type SerializeTupleStruct = JsonVecSerializer;
  type SerializeTupleVariant = JsonTupleVariantSerializer;
  type SerializeMap = JsonMapSerializer;
  type SerializeStruct = JsonMapSerializer;
  type SerializeStructVariant = JsonStructVariantSerializer;

  fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Bool(v))
  }

  fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
    if is_unsafe_js_integer(i128::from(v)) {
      Ok(bigint_value(v))
    } else {
      Ok(JsonValue::Number(v.into()))
    }
  }

  fn serialize_i128(self, v: i128) -> Result<Self::Ok, Self::Error> {
    if is_unsafe_js_integer(v) {
      Ok(bigint_value(v))
    } else {
      Ok(JsonValue::Number((v as i64).into()))
    }
  }

  fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Number(v.into()))
  }

  fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
    if i128::from(v) > JS_MAX_SAFE_INT {
      Ok(bigint_value(v))
    } else {
      Ok(JsonValue::Number(v.into()))
    }
  }

  fn serialize_u128(self, v: u128) -> Result<Self::Ok, Self::Error> {
    if v > JS_MAX_SAFE_INT as u128 {
      Ok(bigint_value(v))
    } else {
      Ok(JsonValue::Number((v as u64).into()))
    }
  }

  fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
    self.serialize_f64(f64::from(v))
  }

  fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
    if v.is_nan() {
      Ok(non_finite_bigint_value("$bigint_nan", false))
    } else if v.is_infinite() {
      Ok(non_finite_bigint_value(
        "$bigint_infinity",
        v.is_sign_negative(),
      ))
    } else {
      finite_f64_value(v)
    }
  }

  fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::String(v.to_string()))
  }

  fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::String(v.to_string()))
  }

  fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Array(
      v.iter().map(|b| JsonValue::Number((*b).into())).collect(),
    ))
  }

  fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Null)
  }

  fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
    value.serialize(self)
  }

  fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Null)
  }

  fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Null)
  }

  fn serialize_unit_variant(
    self,
    _name: &'static str,
    _variant_index: u32,
    variant: &'static str,
  ) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::String(variant.to_string()))
  }

  fn serialize_newtype_struct<T: ?Sized + Serialize>(
    self,
    _name: &'static str,
    value: &T,
  ) -> Result<Self::Ok, Self::Error> {
    value.serialize(self)
  }

  fn serialize_newtype_variant<T: ?Sized + Serialize>(
    self,
    _name: &'static str,
    _variant_index: u32,
    variant: &'static str,
    value: &T,
  ) -> Result<Self::Ok, Self::Error> {
    let mut object = JsonMap::new();
    object.insert(variant.to_string(), value.serialize(IpcResponseSerializer)?);
    Ok(JsonValue::Object(object))
  }

  fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
    Ok(JsonVecSerializer(Vec::with_capacity(len.unwrap_or(0))))
  }

  fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
    self.serialize_seq(Some(len))
  }

  fn serialize_tuple_struct(
    self,
    _name: &'static str,
    len: usize,
  ) -> Result<Self::SerializeTupleStruct, Self::Error> {
    self.serialize_seq(Some(len))
  }

  fn serialize_tuple_variant(
    self,
    _name: &'static str,
    _variant_index: u32,
    variant: &'static str,
    len: usize,
  ) -> Result<Self::SerializeTupleVariant, Self::Error> {
    Ok(JsonTupleVariantSerializer {
      name: variant,
      values: Vec::with_capacity(len),
    })
  }

  fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
    Ok(JsonMapSerializer {
      map: JsonMap::with_capacity(len.unwrap_or(0)),
      next_key: None,
    })
  }

  fn serialize_struct(
    self,
    _name: &'static str,
    len: usize,
  ) -> Result<Self::SerializeStruct, Self::Error> {
    self.serialize_map(Some(len))
  }

  fn serialize_struct_variant(
    self,
    _name: &'static str,
    _variant_index: u32,
    variant: &'static str,
    len: usize,
  ) -> Result<Self::SerializeStructVariant, Self::Error> {
    Ok(JsonStructVariantSerializer {
      name: variant,
      map: JsonMap::with_capacity(len),
    })
  }
}

struct JsonVecSerializer(Vec<JsonValue>);

impl SerializeSeq for JsonVecSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
    self.0.push(value.serialize(IpcResponseSerializer)?);
    Ok(())
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Array(self.0))
  }
}

impl SerializeTuple for JsonVecSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
    SerializeSeq::serialize_element(self, value)
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    SerializeSeq::end(self)
  }
}

impl SerializeTupleStruct for JsonVecSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
    SerializeSeq::serialize_element(self, value)
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    SerializeSeq::end(self)
  }
}

struct JsonTupleVariantSerializer {
  name: &'static str,
  values: Vec<JsonValue>,
}

impl SerializeTupleVariant for JsonTupleVariantSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
    self.values.push(value.serialize(IpcResponseSerializer)?);
    Ok(())
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    let mut object = JsonMap::new();
    object.insert(self.name.to_string(), JsonValue::Array(self.values));
    Ok(JsonValue::Object(object))
  }
}

struct JsonMapSerializer {
  map: JsonMap<String, JsonValue>,
  next_key: Option<String>,
}

impl JsonMapSerializer {
  fn serialize_key_to_string<T: ?Sized + Serialize>(key: &T) -> Result<String, serde_json::Error> {
    match key.serialize(IpcResponseSerializer)? {
      JsonValue::String(key) => Ok(key),
      JsonValue::Number(key) => Ok(key.to_string()),
      JsonValue::Bool(key) => Ok(key.to_string()),
      JsonValue::Null => Ok("null".into()),
      _ => Err(ser::Error::custom(
        "map key must be a string, number, bool, or null",
      )),
    }
  }
}

impl SerializeMap for JsonMapSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
    self.next_key = Some(Self::serialize_key_to_string(key)?);
    Ok(())
  }

  fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
    let key = self
      .next_key
      .take()
      .ok_or_else(|| ser::Error::custom("serialize_value called before serialize_key"))?;
    self
      .map
      .insert(key, value.serialize(IpcResponseSerializer)?);
    Ok(())
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Object(self.map))
  }
}

impl SerializeStruct for JsonMapSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_field<T: ?Sized + Serialize>(
    &mut self,
    key: &'static str,
    value: &T,
  ) -> Result<(), Self::Error> {
    self
      .map
      .insert(key.to_string(), value.serialize(IpcResponseSerializer)?);
    Ok(())
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    Ok(JsonValue::Object(self.map))
  }
}

struct JsonStructVariantSerializer {
  name: &'static str,
  map: JsonMap<String, JsonValue>,
}

impl SerializeStructVariant for JsonStructVariantSerializer {
  type Ok = JsonValue;
  type Error = serde_json::Error;

  fn serialize_field<T: ?Sized + Serialize>(
    &mut self,
    key: &'static str,
    value: &T,
  ) -> Result<(), Self::Error> {
    self
      .map
      .insert(key.to_string(), value.serialize(IpcResponseSerializer)?);
    Ok(())
  }

  fn end(self) -> Result<Self::Ok, Self::Error> {
    let mut object = JsonMap::new();
    object.insert(self.name.to_string(), JsonValue::Object(self.map));
    Ok(JsonValue::Object(object))
  }
}

impl<T: Serialize> IpcResponse for T {
  fn body(self) -> crate::Result<InvokeResponseBody> {
    let value = self.serialize(IpcResponseSerializer)?;
    serde_json::to_string(&value)
      .map(Into::into)
      .map_err(Into::into)
  }
}

/// The IPC response.
pub struct Response {
  body: InvokeResponseBody,
}

impl IpcResponse for Response {
  fn body(self) -> crate::Result<InvokeResponseBody> {
    Ok(self.body)
  }
}

impl Response {
  /// Defines a response with the given body.
  pub fn new(body: impl Into<InvokeResponseBody>) -> Self {
    Self { body: body.into() }
  }
}

/// The message and resolver given to a custom command.
///
/// This struct is used internally by macros and is explicitly **NOT** stable.
#[default_runtime(crate::Wry, wry)]
pub struct Invoke<R: Runtime> {
  /// The message passed.
  pub message: InvokeMessage<R>,

  /// The resolver of the message.
  pub resolver: InvokeResolver<R>,

  /// Resolved ACL for this IPC invoke.
  pub acl: Option<Vec<ResolvedCommand>>,
}

/// Error response from an [`InvokeMessage`].
#[derive(Debug)]
pub struct InvokeError(pub serde_json::Value);

impl InvokeError {
  /// Create an [`InvokeError`] as a string of the [`std::error::Error`] message.
  #[inline(always)]
  pub fn from_error<E: std::error::Error>(error: E) -> Self {
    Self(serde_json::Value::String(error.to_string()))
  }

  /// Create an [`InvokeError`] as a string of the [`anyhow::Error`] message.
  #[inline(always)]
  pub fn from_anyhow(error: anyhow::Error) -> Self {
    Self(serde_json::Value::String(format!("{error:#}")))
  }
}

impl<T: Serialize> From<T> for InvokeError {
  #[inline]
  fn from(value: T) -> Self {
    serde_json::to_value(value)
      .map(Self)
      .unwrap_or_else(Self::from_error)
  }
}

impl From<crate::Error> for InvokeError {
  #[inline(always)]
  fn from(error: crate::Error) -> Self {
    Self(serde_json::Value::String(error.to_string()))
  }
}

/// Response from a [`InvokeMessage`] passed to the [`InvokeResolver`].
#[derive(Debug)]
pub enum InvokeResponse {
  /// Resolve the promise.
  Ok(InvokeResponseBody),
  /// Reject the promise.
  Err(InvokeError),
}

impl<T: IpcResponse, E: Into<InvokeError>> From<Result<T, E>> for InvokeResponse {
  #[inline]
  fn from(result: Result<T, E>) -> Self {
    match result {
      Ok(ok) => match ok.body() {
        Ok(value) => Self::Ok(value),
        Err(err) => Self::Err(InvokeError::from_error(err)),
      },
      Err(err) => Self::Err(err.into()),
    }
  }
}

impl From<InvokeError> for InvokeResponse {
  fn from(error: InvokeError) -> Self {
    Self::Err(error)
  }
}

/// Resolver of a invoke message.
#[default_runtime(crate::Wry, wry)]
pub struct InvokeResolver<R: Runtime> {
  webview: Webview<R>,
  responder: Arc<Mutex<Option<Box<OwnedInvokeResponder<R>>>>>,
  cmd: String,
  pub(crate) callback: CallbackFn,
  pub(crate) error: CallbackFn,
}

impl<R: Runtime> Clone for InvokeResolver<R> {
  fn clone(&self) -> Self {
    Self {
      webview: self.webview.clone(),
      responder: self.responder.clone(),
      cmd: self.cmd.clone(),
      callback: self.callback,
      error: self.error,
    }
  }
}

impl<R: Runtime> InvokeResolver<R> {
  pub(crate) fn new(
    webview: Webview<R>,
    responder: Arc<Mutex<Option<Box<OwnedInvokeResponder<R>>>>>,
    cmd: String,
    callback: CallbackFn,
    error: CallbackFn,
  ) -> Self {
    Self {
      webview,
      responder,
      cmd,
      callback,
      error,
    }
  }

  /// Reply to the invoke promise with an async task.
  pub fn respond_async<T, F>(self, task: F)
  where
    T: IpcResponse,
    F: Future<Output = Result<T, InvokeError>> + Send + 'static,
  {
    crate::async_runtime::spawn(async move {
      Self::return_task(
        self.webview,
        self.responder,
        task,
        self.cmd,
        self.callback,
        self.error,
      )
      .await;
    });
  }

  /// Reply to the invoke promise with an async task which is already serialized.
  pub fn respond_async_serialized<F>(self, task: F)
  where
    F: Future<Output = Result<InvokeResponseBody, InvokeError>> + Send + 'static,
  {
    // Dynamic dispatch the call in dev for a faster compile time
    // TODO: Revisit this and see if we can do this for the release build as well if the performance hit is not a problem
    #[cfg(debug_assertions)]
    {
      self.respond_async_serialized_dyn(Box::pin(task))
    }
    #[cfg(not(debug_assertions))]
    {
      self.respond_async_serialized_inner(task)
    }
  }

  /// Dynamic dispatch the [`Self::respond_async_serialized`] call
  #[cfg(debug_assertions)]
  fn respond_async_serialized_dyn(
    self,
    task: std::pin::Pin<
      Box<dyn Future<Output = Result<InvokeResponseBody, InvokeError>> + Send + 'static>,
    >,
  ) {
    self.respond_async_serialized_inner(task)
  }

  /// Reply to the invoke promise with an async task which is already serialized.
  fn respond_async_serialized_inner<F>(self, task: F)
  where
    F: Future<Output = Result<InvokeResponseBody, InvokeError>> + Send + 'static,
  {
    crate::async_runtime::spawn(async move {
      let response = match task.await {
        Ok(ok) => InvokeResponse::Ok(ok),
        Err(err) => InvokeResponse::Err(err),
      };
      Self::return_result(
        self.webview,
        self.responder,
        response,
        self.cmd,
        self.callback,
        self.error,
      )
    });
  }

  /// Reply to the invoke promise with a serializable value.
  pub fn respond<T: IpcResponse>(self, value: Result<T, InvokeError>) {
    Self::return_result(
      self.webview,
      self.responder,
      value.into(),
      self.cmd,
      self.callback,
      self.error,
    )
  }

  /// Resolve the invoke promise with a value.
  pub fn resolve<T: IpcResponse>(self, value: T) {
    self.respond(Ok(value))
  }

  /// Reject the invoke promise with a value.
  pub fn reject<T: Serialize>(self, value: T) {
    Self::return_result(
      self.webview,
      self.responder,
      Result::<(), _>::Err(value).into(),
      self.cmd,
      self.callback,
      self.error,
    )
  }

  /// Reject the invoke promise with an [`InvokeError`].
  pub fn invoke_error(self, error: InvokeError) {
    Self::return_result(
      self.webview,
      self.responder,
      error.into(),
      self.cmd,
      self.callback,
      self.error,
    )
  }

  /// Asynchronously executes the given task
  /// and evaluates its Result to the JS promise described by the `success_callback` and `error_callback` function names.
  ///
  /// If the Result `is_ok()`, the callback will be the `success_callback` function name and the argument will be the Ok value.
  /// If the Result `is_err()`, the callback will be the `error_callback` function name and the argument will be the Err value.
  pub async fn return_task<T, F>(
    webview: Webview<R>,
    responder: Arc<Mutex<Option<Box<OwnedInvokeResponder<R>>>>>,
    task: F,
    cmd: String,
    success_callback: CallbackFn,
    error_callback: CallbackFn,
  ) where
    T: IpcResponse,
    F: Future<Output = Result<T, InvokeError>> + Send + 'static,
  {
    let result = task.await;
    Self::return_closure(
      webview,
      responder,
      || result,
      cmd,
      success_callback,
      error_callback,
    )
  }

  pub(crate) fn return_closure<T: IpcResponse, F: FnOnce() -> Result<T, InvokeError>>(
    webview: Webview<R>,
    responder: Arc<Mutex<Option<Box<OwnedInvokeResponder<R>>>>>,
    f: F,
    cmd: String,
    success_callback: CallbackFn,
    error_callback: CallbackFn,
  ) {
    Self::return_result(
      webview,
      responder,
      f().into(),
      cmd,
      success_callback,
      error_callback,
    )
  }

  pub(crate) fn return_result(
    webview: Webview<R>,
    responder: Arc<Mutex<Option<Box<OwnedInvokeResponder<R>>>>>,
    response: InvokeResponse,
    cmd: String,
    success_callback: CallbackFn,
    error_callback: CallbackFn,
  ) {
    (responder.lock().unwrap().take().expect("resolver consumed"))(
      webview,
      cmd,
      response,
      success_callback,
      error_callback,
    );
  }
}

/// An invoke message.
#[default_runtime(crate::Wry, wry)]
#[derive(Debug)]
pub struct InvokeMessage<R: Runtime> {
  /// The webview that received the invoke message.
  pub(crate) webview: Webview<R>,
  /// Application managed state.
  pub(crate) state: Arc<StateManager>,
  /// The IPC command.
  pub(crate) command: String,
  /// The JSON argument passed on the invoke message.
  pub(crate) payload: InvokeBody,
  /// The request headers.
  pub(crate) headers: HeaderMap,
}

impl<R: Runtime> Clone for InvokeMessage<R> {
  fn clone(&self) -> Self {
    Self {
      webview: self.webview.clone(),
      state: self.state.clone(),
      command: self.command.clone(),
      payload: self.payload.clone(),
      headers: self.headers.clone(),
    }
  }
}

impl<R: Runtime> InvokeMessage<R> {
  /// Create an new [`InvokeMessage`] from a payload send by a webview.
  pub(crate) fn new(
    webview: Webview<R>,
    state: Arc<StateManager>,
    command: String,
    payload: InvokeBody,
    headers: HeaderMap,
  ) -> Self {
    Self {
      webview,
      state,
      command,
      payload,
      headers,
    }
  }

  /// The invoke command.
  #[inline(always)]
  pub fn command(&self) -> &str {
    &self.command
  }

  /// The webview that received the invoke.
  #[inline(always)]
  pub fn webview(&self) -> Webview<R> {
    self.webview.clone()
  }

  /// A reference to webview that received the invoke.
  #[inline(always)]
  pub fn webview_ref(&self) -> &Webview<R> {
    &self.webview
  }

  /// A reference to the payload the invoke received.
  #[inline(always)]
  pub fn payload(&self) -> &InvokeBody {
    &self.payload
  }

  /// The state manager associated with the application
  #[inline(always)]
  pub fn state(&self) -> Arc<StateManager> {
    self.state.clone()
  }

  /// A reference to the state manager associated with application.
  #[inline(always)]
  pub fn state_ref(&self) -> &StateManager {
    &self.state
  }

  /// The request headers.
  #[inline(always)]
  pub fn headers(&self) -> &HeaderMap {
    &self.headers
  }
}

/// The `Callback` type is the return value of the `transformCallback` JavaScript function.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct CallbackFn(pub u32);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn deserialize_invoke_response_body() {
    let json = InvokeResponseBody::Json("[1, 123, 1231]".to_string());
    assert_eq!(json.deserialize::<Vec<u16>>().unwrap(), vec![1, 123, 1231]);

    let json = InvokeResponseBody::Json("\"string value\"".to_string());
    assert_eq!(json.deserialize::<String>().unwrap(), "string value");

    let json = InvokeResponseBody::Json("\"string value\"".to_string());
    assert!(json.deserialize::<Vec<u16>>().is_err());

    let values = vec![1, 2, 3, 4, 5, 6, 1];
    let raw = InvokeResponseBody::Raw(values.clone());
    assert_eq!(raw.deserialize::<Vec<u8>>().unwrap(), values);
  }

  #[test]
  fn ipc_response_serializes_bigints_specially() {
    #[derive(Serialize)]
    struct Response {
      safe: i64,
      unsafe_integer: i64,
      min_i128: i128,
      nan: f64,
      infinity: f64,
      negative_infinity: f64,
    }

    let response = Response {
      safe: 9_007_199_254_740_991,
      unsafe_integer: 9_007_199_254_740_992,
      min_i128: i128::MIN,
      nan: f64::NAN,
      infinity: f64::INFINITY,
      negative_infinity: f64::NEG_INFINITY,
    }
    .body()
    .unwrap();

    let InvokeResponseBody::Json(response) = response else {
      panic!("expected json response");
    };

    assert_eq!(
      serde_json::from_str::<JsonValue>(&response).unwrap(),
      serde_json::json!({
        "safe": 9_007_199_254_740_991_i64,
        "unsafe_integer": { "$bigint": "9007199254740992" },
        "min_i128": { "$bigint": i128::MIN.to_string() },
        "nan": { "$bigint": true, "$bigint_nan": true },
        "infinity": { "$bigint": true, "$bigint_infinity": true },
        "negative_infinity": {
          "$bigint": true,
          "$bigint_infinity": true,
          "$bigint_negative_infinity": true,
          "$bigint_negative": true
        }
      })
    );
  }
}
