//! Transformers wrap a part of or the entirety of [`OpenApi`]
//! and enable modifications with method chaining in a declarative way.
//!
//! # Examples
//!
//! Example documentation for an imaginary government-provided API:
//!
//! ```ignore
//! op.description("An example operation.")
//!     .response_with::<200, Json<String>, _>(|res| {
//!         res.description(
//!             "Something was probably successful, we don't know \
//!                     what this returns, but it's at least JSON.",
//!         )
//!     })
//!     .response_with::<500, Html<String>, _>(|res| {
//!         res.description("Sometimes arbitrary 500 is returned with randomized HTML.")
//!     })
//!     .default_response::<String>()
//! ```
//!
//! # Transform Functions
//!
//! Transform functions are functions that accept a single
//! transformer as a parameter and return it. They
//! enable composability of documentation.
//!
//! ## Example
//!
//! ```rust
//! # use aide::transform::TransformOperation;
//! /// This transform function simply adds a no-content response as an example.
//! fn no_content(op: TransformOperation) -> TransformOperation {
//!     op.response::<204, ()>()
//! }
//! ```
//!
//! The above then can be (re)used using `with`.
//!
//! ```
//! # use aide::transform::TransformOperation;
//! # fn no_content(op: TransformOperation) -> TransformOperation {
//!     op.description("this operation always returns nothing")
//!         .with(no_content)
//! # }
//! ```
//!

use std::{
    any::type_name,
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
};

use crate::{
    generate::GenContext,
    openapi::{
        Components, Contact, Info, License, MediaType, OpenApi, Operation, Parameter,
        ParameterSchemaOrContent, PathItem, ReferenceOr, Response, SchemaObject, SecurityScheme,
        Server, StatusCode, Tag,
    },
    OperationInput,
};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;

use crate::{
    error::Error, generate::in_context, operation::OperationOutput, util::iter_operations_mut,
};

/// A transform helper that wraps [`OpenApi`].
#[must_use]
pub struct TransformOpenApi<'t> {
    pub(crate) api: &'t mut OpenApi,
}

impl<'t> TransformOpenApi<'t> {
    /// Create a new transform helper.
    pub fn new(api: &'t mut OpenApi) -> Self {
        Self { api }
    }

    /// Set the title.
    #[tracing::instrument(skip_all)]
    pub fn title(self, title: &str) -> Self {
        self.api.info.title = title.into();
        self
    }

    /// Set the summary.
    #[tracing::instrument(skip_all)]
    pub fn summary(self, summary: &str) -> Self {
        self.api.info.summary = Some(summary.into());
        self
    }

    /// Set the terms of service.
    #[tracing::instrument(skip_all)]
    pub fn tos(self, tos: &str) -> Self {
        self.api.info.terms_of_service = Some(tos.into());
        self
    }

    /// Set the description.
    #[tracing::instrument(skip_all)]
    pub fn description(self, description: &str) -> Self {
        self.api.info.description = Some(description.into());
        self
    }

    /// Set the version.
    #[tracing::instrument(skip_all)]
    pub fn version(self, version: &str) -> Self {
        self.api.info.version = version.into();
        self
    }

    /// Set API contact information.
    #[tracing::instrument(skip_all)]
    pub fn contact(self, contact: Contact) -> Self {
        self.api.info.contact = Some(contact);
        self
    }

    /// Set API license information.
    #[tracing::instrument(skip_all)]
    pub fn license(self, license: License) -> Self {
        self.api.info.license = Some(license);
        self
    }

    /// Override all API information.
    #[tracing::instrument(skip_all)]
    pub fn info(self, info: Info) -> Self {
        self.api.info = info;
        self
    }

    /// Add a tag to the documentation.
    #[tracing::instrument(skip_all)]
    pub fn tag(self, tag: Tag) -> Self {
        self.api.tags.push(tag);
        self
    }

    /// Add a server to the documentation.
    #[tracing::instrument(skip_all)]
    pub fn server(self, server: Server) -> Self {
        self.api.servers.push(server);
        self
    }

    /// Set a default response for all operations
    /// that do not already have one.
    #[tracing::instrument(skip_all)]
    pub fn default_response<R>(self) -> Self
    where
        R: OperationOutput,
    {
        if let Some(p) = &mut self.api.paths {
            for (_, p) in &mut p.paths {
                let p = match p {
                    ReferenceOr::Reference { .. } => continue,
                    ReferenceOr::Item(p) => p,
                };

                let _ = TransformPathItem::new(p).default_response::<R>();
            }
        }

        self
    }

    /// Set a default response for all operations
    /// that do not already have one.
    ///
    /// This method additionally accepts a transform function
    /// to modify the generated documentation.
    #[tracing::instrument(skip_all)]
    pub fn default_response_with<R, F>(self, transform: F) -> Self
    where
        R: OperationOutput,
        F: Fn(TransformResponse<'_, R::Inner>) -> TransformResponse<'_, R::Inner> + Clone,
    {
        if let Some(p) = &mut self.api.paths {
            for (_, p) in &mut p.paths {
                let p = match p {
                    ReferenceOr::Reference { .. } => continue,
                    ReferenceOr::Item(p) => p,
                };

                for (_, op) in iter_operations_mut(p) {
                    let _ = TransformOperation::new(op)
                        .default_response_with::<R, F>(transform.clone());
                }
            }
        }

        self
    }

    /// Strip null types from query parameter schemas.
    ///
    /// Query strings cannot express null values - a parameter is either
    /// present with a value or absent. This method removes `null` from
    /// type arrays (e.g., `["string", "null"]` becomes `"string"`) and
    /// unwraps `anyOf` variants containing null types.
    ///
    /// Note: This is called automatically when the transform is finalized
    /// (unless disabled via [`aide::generate::strip_query_null_types`]).
    /// You only need to call this explicitly if you want to apply it
    /// at a specific point in your transform chain.
    #[tracing::instrument(skip_all)]
    pub fn strip_null_from_query_params(self) -> Self {
        strip_null_from_query_params_impl(self.api);
        self
    }

    /// Add a security scheme.
    #[allow(clippy::missing_panics_doc)]
    pub fn security_scheme(mut self, name: &str, scheme: SecurityScheme) -> Self {
        let components = match &mut self.inner_mut().components {
            Some(c) => c,
            None => {
                self.inner_mut().components = Some(Components::default());
                self.inner_mut().components.as_mut().unwrap()
            }
        };

        components
            .security_schemes
            .insert(name.into(), ReferenceOr::Item(scheme));

        self
    }

    /// Add a global security requirement.
    #[tracing::instrument(skip_all)]
    pub fn security_requirement(self, security_scheme: &str) -> Self {
        self.security_requirement_multi([security_scheme])
    }

    /// Add multiple global security requirement.
    #[tracing::instrument(skip_all)]
    pub fn security_requirement_multi<'a, I>(mut self, security_schemes: I) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
    {
        if self.inner_mut().security.iter().any(|s| {
            s.len() == security_schemes.clone().into_iter().count()
                && security_schemes
                    .clone()
                    .into_iter()
                    .all(|security_scheme| s.contains_key(security_scheme))
        }) {
            return self;
        }

        self.inner_mut().security.push(
            security_schemes
                .into_iter()
                .map(|security_scheme| (security_scheme.to_string(), Vec::new()))
                .collect(),
        );

        self
    }

    /// Add required scopes to a global security requirement.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all)]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_scopes<I, S>(self, security_scheme: &str, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.security_requirement_multi_scopes([security_scheme], scopes)
    }

    /// Add required scopes to multi global security requirement.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all)]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_multi_scopes<'a, I, IS, S>(
        mut self,
        security_schemes: I,
        scopes: IS,
    ) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
        IS: IntoIterator<Item = S>,
        S: Into<String>,
    {
        match self.inner_mut().security.iter_mut().find(|s| {
            s.len() == security_schemes.clone().into_iter().count()
                && security_schemes
                    .clone()
                    .into_iter()
                    .all(|security_scheme| s.contains_key(security_scheme))
        }) {
            Some(s) => {
                let scopes: Vec<String> = scopes.into_iter().map(Into::into).collect();
                s.iter_mut().for_each(|(_, s)| s.extend(scopes.clone()));
            }
            None => {
                let scopes: Vec<String> = scopes.into_iter().map(Into::into).collect();
                self.inner_mut().security.push(
                    security_schemes
                        .into_iter()
                        .map(|security_scheme| (security_scheme.to_string(), scopes.clone()))
                        .collect(),
                );
            }
        }

        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Access the inner [`OpenApi`].
    #[inline]
    pub fn inner_mut(&mut self) -> &mut OpenApi {
        self.api
    }
}

/// A transform helper that wraps [`TransformPathItem`].
#[must_use]
pub struct TransformPathItem<'t> {
    pub(crate) hidden: bool,
    pub(crate) path: &'t mut PathItem,
}

impl<'t> TransformPathItem<'t> {
    /// Create a new transform helper.
    pub fn new(path: &'t mut PathItem) -> Self {
        Self {
            hidden: false,
            path,
        }
    }

    /// Hide the path from the documentation.
    ///
    /// This is taken into account by generators provided
    /// by this library.
    ///
    /// Hiding an item causes it to be ignored
    /// completely, there is no way to restore or "unhide" it afterwards.
    #[tracing::instrument(skip_all)]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Provide a summary for the path.
    #[tracing::instrument(skip_all)]
    pub fn summary(self, desc: &str) -> Self {
        self.path.summary = Some(desc.into());
        self
    }

    /// Provide a description for the path.
    #[tracing::instrument(skip_all)]
    pub fn description(self, desc: &str) -> Self {
        self.path.description = Some(desc.into());
        self
    }

    /// Add a tag to all operations.
    #[tracing::instrument(skip_all)]
    pub fn tag(self, tag: &str) -> Self {
        for (_, op) in iter_operations_mut(self.path) {
            if !op.tags.iter().any(|t| t == tag) {
                op.tags.push(tag.into());
            }
        }

        self
    }

    /// Set a default response for all operations in the
    /// path that do not already have one.
    #[tracing::instrument(skip_all)]
    pub fn default_response<R>(self) -> Self
    where
        R: OperationOutput,
    {
        in_context(|ctx| ctx.show_error = filter_no_duplicate_response);

        for (_, op) in iter_operations_mut(self.path) {
            let _ = TransformOperation::new(op).default_response::<R>();
        }

        in_context(GenContext::reset_error_filter);

        self
    }

    /// Set a default response for all operations in the
    /// path that do not already have one.
    ///
    /// This method additionally accepts a transform function
    /// to modify the generated documentation.
    #[tracing::instrument(skip_all)]
    pub fn default_response_with<R, F>(self, transform: F) -> Self
    where
        R: OperationOutput,
        F: Fn(TransformResponse<'_, R::Inner>) -> TransformResponse<'_, R::Inner> + Clone,
    {
        in_context(|ctx| ctx.show_error = filter_no_duplicate_response);

        for (_, op) in iter_operations_mut(self.path) {
            let _ = TransformOperation::new(op).default_response_with::<R, F>(transform.clone());
        }

        in_context(GenContext::reset_error_filter);

        self
    }

    /// Add a security requirement for all operations.
    #[tracing::instrument(skip_all)]
    pub fn security_requirement(self, security_scheme: &str) -> Self {
        self.security_requirement_multi([security_scheme])
    }

    /// Add multi security requirement for all operations.
    #[tracing::instrument(skip_all)]
    pub fn security_requirement_multi<'a, I>(self, security_schemes: I) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
    {
        for (_, op) in iter_operations_mut(self.path) {
            let _ =
                TransformOperation::new(op).security_requirement_multi(security_schemes.clone());
        }

        self
    }

    /// Add required scopes to a security requirement for all operations.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all)]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_scopes<I, S>(self, security_scheme: &str, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.security_requirement_multi_scopes([security_scheme], scopes)
    }

    /// Add required scopes to a security requirement for all operations.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all)]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_multi_scopes<'a, I, IS, S>(
        self,
        security_schemes: I,
        scopes: IS,
    ) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
        IS: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let scopes: Vec<String> = scopes.into_iter().map(Into::into).collect();

        for (_, op) in iter_operations_mut(self.path) {
            let _ = TransformOperation::new(op)
                .security_requirement_multi_scopes(security_schemes.clone(), scopes.clone());
        }

        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Access the inner [`PathItem`].
    #[inline]
    pub fn inner_mut(&mut self) -> &mut PathItem {
        self.path
    }
}

/// A transform helper that wraps [`Operation`].
#[must_use]
pub struct TransformOperation<'t> {
    pub(crate) hidden: bool,
    pub(crate) operation: &'t mut Operation,
}

impl<'t> TransformOperation<'t> {
    /// Create a new transform helper.
    pub fn new(operation: &'t mut Operation) -> Self {
        Self {
            hidden: false,
            operation,
        }
    }

    /// Specify the operation ID.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn id(self, name: &str) -> Self {
        self.operation.operation_id = Some(name.into());
        self
    }

    /// Provide a summary for the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn summary(self, desc: &str) -> Self {
        self.operation.summary = Some(desc.into());
        self
    }

    /// Provide a description for the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn description(self, desc: &str) -> Self {
        self.operation.description = Some(desc.into());
        self
    }

    /// Add a tag to this operation.
    #[tracing::instrument(skip_all)]
    pub fn tag(self, tag: &str) -> Self {
        if !self.operation.tags.iter().any(|t| t == tag) {
            self.operation.tags.push(tag.into());
        }

        self
    }

    /// Hide the operation from the documentation.
    ///
    /// This is taken into account by generators provided
    /// by this library.
    ///
    /// Hiding an item causes it to be ignored
    /// completely, there is no way to restore or "unhide" it afterwards.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Add input (parameters or request body) to the operation.
    ///
    /// The type parameter can be a single type
    /// or a tuple of types that implement [`OperationInput`].
    ///
    /// This function is automatically called for
    /// request handlers of supported web frameworks.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn input<T: OperationInput>(self) -> Self {
        in_context(|ctx| {
            T::operation_input(ctx, self.operation);
        });
        self
    }

    /// Modify a parameter of the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn parameter<T, F>(self, name: &str, transform: F) -> Self
    where
        T: Serialize,
        F: FnOnce(TransformParameter<'_, T>) -> TransformParameter<'_, T>,
    {
        let (idx, param) = match self
            .operation
            .parameters
            .iter_mut()
            .enumerate()
            .find(|(_, p)| match p {
                ReferenceOr::Item(p) => p.parameter_data_ref().name == name,
                ReferenceOr::Reference { .. } => false,
            }) {
            Some((idx, p)) => match p {
                ReferenceOr::Item(p) => (idx, p),
                ReferenceOr::Reference { .. } => {
                    in_context(|ctx| {
                        ctx.error(Error::UnexpectedReference);
                    });
                    return self;
                }
            },
            None => {
                in_context(|ctx| {
                    ctx.error(Error::ParameterNotExists(name.to_string()));
                });
                return self;
            }
        };

        let t = transform(TransformParameter::new(param));

        if t.hidden {
            self.operation.parameters.remove(idx);
        }

        self
    }

    /// Modify a parameter of the operation without knowing a type.
    ///
    /// The type `()` will be used instead.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn parameter_untyped<F>(self, name: &str, transform: F) -> Self
    where
        F: FnOnce(TransformParameter<'_, ()>) -> TransformParameter<'_, ()>,
    {
        self.parameter(name, transform)
    }

    /// Set a default response for the operation if
    /// it does not already have one.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn default_response<R>(self) -> Self
    where
        R: OperationOutput,
    {
        if self.operation.responses.is_none() {
            self.operation.responses = Some(Default::default());
        }

        in_context(|ctx| {
            if let Some(res) = R::operation_response(ctx, self.operation) {
                let responses = self.operation.responses.as_mut().unwrap();
                if responses.default.is_none() {
                    responses.default = Some(ReferenceOr::Item(res));
                } else {
                    ctx.error(Error::DefaultResponseExists);
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Set a default response for the operation if
    /// it does not already have one.
    ///
    /// This method additionally accepts a transform function
    /// to modify the generated documentation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn default_response_with<R, F>(self, transform: F) -> Self
    where
        R: OperationOutput,
        F: FnOnce(TransformResponse<'_, R::Inner>) -> TransformResponse<'_, R::Inner>,
    {
        in_context(|ctx| {
            if let Some(mut res) = R::operation_response(ctx, self.operation) {
                let responses = self
                    .operation
                    .responses
                    .get_or_insert_with(Default::default);
                if responses.default.is_none() {
                    let t = transform(TransformResponse::new(&mut res));

                    if !t.hidden {
                        responses.default = Some(ReferenceOr::Item(res));
                    }
                } else {
                    ctx.error(Error::DefaultResponseExists);
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Add a response to the operation with the given status code.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn response<const N: u16, R>(self) -> Self
    where
        R: OperationOutput,
    {
        if self.operation.responses.is_none() {
            self.operation.responses = Some(Default::default());
        }

        in_context(|ctx| {
            if let Some(res) = R::operation_response(ctx, self.operation) {
                let responses = self.operation.responses.as_mut().unwrap();
                if responses
                    .responses
                    .insert(StatusCode::Code(N), ReferenceOr::Item(res))
                    .is_some()
                {
                    ctx.error(Error::ResponseExists(StatusCode::Code(N)));
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Add a response to the operation with the given status code.
    ///
    /// This method additionally accepts a transform function
    /// to modify the generated documentation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn response_with<const N: u16, R, F>(self, transform: F) -> Self
    where
        R: OperationOutput,
        F: FnOnce(TransformResponse<'_, R::Inner>) -> TransformResponse<'_, R::Inner>,
    {
        if self.operation.responses.is_none() {
            self.operation.responses = Some(Default::default());
        }

        in_context(|ctx| {
            if let Some(mut res) = R::operation_response(ctx, self.operation) {
                let t = transform(TransformResponse::new(&mut res));

                let responses = self.operation.responses.as_mut().unwrap();
                if !t.hidden {
                    let existing = responses
                        .responses
                        .insert(StatusCode::Code(N), ReferenceOr::Item(res))
                        .is_some();
                    if existing {
                        ctx.error(Error::ResponseExists(StatusCode::Code(N)));
                    }
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Add a response to the operation with the given status code range (e.g. 2xx).
    ///
    /// Note that the range is `100`-based, so for the range `2xx`, `2` must be provided.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn response_range<const N: u16, R>(self) -> Self
    where
        R: OperationOutput,
    {
        if self.operation.responses.is_none() {
            self.operation.responses = Some(Default::default());
        }

        in_context(|ctx| {
            if let Some(res) = R::operation_response(ctx, self.operation) {
                let responses = self.operation.responses.as_mut().unwrap();
                if responses
                    .responses
                    .insert(StatusCode::Range(N), ReferenceOr::Item(res))
                    .is_some()
                {
                    ctx.error(Error::ResponseExists(StatusCode::Range(N)));
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Add a response to the operation with the given status code range (e.g. 2xx).
    ///
    /// Note that the range is `100`-based, so for the range `2xx`, `2` must be provided.
    ///
    /// This method additionally accepts a transform function
    /// to modify the generated documentation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn response_range_with<const N: u16, R, F>(self, transform: F) -> Self
    where
        R: OperationOutput,
        F: FnOnce(TransformResponse<'_, R::Inner>) -> TransformResponse<'_, R::Inner>,
    {
        if self.operation.responses.is_none() {
            self.operation.responses = Some(Default::default());
        }

        in_context(|ctx| {
            if let Some(mut res) = R::operation_response(ctx, self.operation) {
                let t = transform(TransformResponse::new(&mut res));

                let responses = self.operation.responses.as_mut().unwrap();
                if !t.hidden {
                    let existing = responses
                        .responses
                        .insert(StatusCode::Range(N), ReferenceOr::Item(res))
                        .is_some();
                    if existing {
                        ctx.error(Error::ResponseExists(StatusCode::Range(N)));
                    }
                }
            } else {
                tracing::debug!(type_name = type_name::<R>(), "no response info of type");
            }
        });

        self
    }

    /// Add a callback to the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn callback(
        self,
        callback_name: &str,
        callback_url: &str,
        callback_transform: impl FnOnce(TransformCallback<'_>) -> TransformCallback<'_>,
    ) -> Self {
        let callbacks = self
            .operation
            .callbacks
            .entry(callback_name.to_string())
            .or_insert_with(|| ReferenceOr::Item(IndexMap::default()));

        let callbacks = match callbacks {
            ReferenceOr::Reference { .. } => {
                in_context(|ctx| ctx.error(Error::UnexpectedReference));
                return self;
            }
            ReferenceOr::Item(cbs) => cbs,
        };

        let p = callbacks
            .entry(callback_url.to_string())
            .or_insert_with(|| ReferenceOr::Item(PathItem::default()));

        let p = match p {
            ReferenceOr::Reference { .. } => {
                in_context(|ctx| ctx.error(Error::UnexpectedReference));
                return self;
            }
            ReferenceOr::Item(p) => p,
        };

        let t = callback_transform(TransformCallback::new(p));

        if t.hidden {
            callbacks.swap_remove(callback_url);
            if self
                .operation
                .callbacks
                .get(callback_name)
                .unwrap()
                .as_item()
                .unwrap()
                .is_empty()
            {
                self.operation.callbacks.swap_remove(callback_name);
            }
        }

        self
    }

    /// Add a security requirement to the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn security_requirement(self, security_scheme: &str) -> Self {
        self.security_requirement_multi([security_scheme])
    }

    /// Add multi security requirement to the operation.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    pub fn security_requirement_multi<'a, I>(self, security_schemes: I) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
    {
        if self.operation.security.iter().any(|s| {
            s.len() == security_schemes.clone().into_iter().count()
                && security_schemes
                    .clone()
                    .into_iter()
                    .all(|security_scheme| s.contains_key(security_scheme))
        }) {
            return self;
        }

        self.operation.security.push(
            security_schemes
                .into_iter()
                .map(|security_scheme| (security_scheme.to_string(), Vec::new()))
                .collect(),
        );

        self
    }

    /// Add required scopes to a security requirement.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_scopes<I, S>(self, security_scheme: &str, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.security_requirement_multi_scopes([security_scheme], scopes)
    }
    /// Add required scopes to multi security requirement.
    ///
    /// If the scheme requirement does not exist,
    /// it will be added.
    #[tracing::instrument(skip_all, fields(operation_id = self.operation.operation_id))]
    #[allow(clippy::missing_panics_doc)]
    pub fn security_requirement_multi_scopes<'a, I, IS, S>(
        self,
        security_schemes: I,
        scopes: IS,
    ) -> Self
    where
        I: IntoIterator<Item = &'a str> + Clone,
        IS: IntoIterator<Item = S>,
        S: Into<String>,
    {
        match self.operation.security.iter_mut().find(|s| {
            s.len() == security_schemes.clone().into_iter().count()
                && security_schemes
                    .clone()
                    .into_iter()
                    .all(|security_scheme| s.contains_key(security_scheme))
        }) {
            Some(s) => {
                let scopes: Vec<String> = scopes.into_iter().map(Into::into).collect();
                s.iter_mut().for_each(|(_, s)| s.extend(scopes.clone()));
            }
            None => {
                let scopes: Vec<String> = scopes.into_iter().map(Into::into).collect();
                self.operation.security.push(
                    security_schemes
                        .into_iter()
                        .map(|security_scheme| (security_scheme.to_string(), scopes.clone()))
                        .collect(),
                );
            }
        }

        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Access the inner [`Operation`].
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Operation {
        self.operation
    }
}

/// A transform helper that wraps [`Parameter`].
///
/// An additional type is provided for strongly-typed
/// examples.
#[must_use]
pub struct TransformParameter<'t, T> {
    pub(crate) hidden: bool,
    pub(crate) param: &'t mut Parameter,
    _t: PhantomData<T>,
}

impl<'t, T> TransformParameter<'t, T> {
    /// Create a new transform helper.
    pub fn new(param: &'t mut Parameter) -> Self {
        Self {
            hidden: false,
            param,
            _t: PhantomData,
        }
    }

    /// Hide the parameter from the documentation.
    ///
    /// This is taken into account by generators provided
    /// by this library.
    ///
    /// Hiding an item causes it to be ignored
    /// completely, there is no way to restore or "unhide" it afterwards.
    #[tracing::instrument(skip_all)]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Provide or override the description of the parameter.
    #[tracing::instrument(skip_all)]
    pub fn description(mut self, desc: &str) -> Self {
        let data = match &mut self.param {
            Parameter::Query { parameter_data, .. }
            | Parameter::Header { parameter_data, .. }
            | Parameter::Path { parameter_data, .. }
            | Parameter::Cookie { parameter_data, .. } => parameter_data,
        };
        data.description = Some(desc.into());
        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Access the inner [`Parameter`].
    #[inline]
    pub fn inner_mut(&mut self) -> &mut Parameter {
        self.param
    }
}

/// A transform helper that wraps [`Response`].
///
/// An additional type is provided for strongly-typed
/// examples.
#[must_use]
pub struct TransformResponse<'t, T> {
    pub(crate) hidden: bool,
    pub(crate) response: &'t mut Response,
    _t: PhantomData<T>,
}

impl<'t, T> TransformResponse<'t, T> {
    /// Create a new transform helper.
    pub fn new(response: &'t mut Response) -> Self {
        Self {
            hidden: false,
            response,
            _t: PhantomData,
        }
    }

    /// Hide the response from the documentation.
    ///
    /// This is taken into account by generators provided
    /// by this library.
    ///
    /// Hiding an item causes it to be ignored
    /// completely, there is no way to restore or "unhide" it afterwards.
    #[tracing::instrument(skip_all)]
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Provide or override the description of the response.
    #[tracing::instrument(skip_all)]
    pub fn description(self, desc: &str) -> Self {
        self.response.description = desc.into();
        self
    }

    /// Provide or override an example for the response.
    #[tracing::instrument(skip_all)]
    #[allow(clippy::missing_panics_doc)]
    pub fn example(self, example: impl Into<T>) -> Self
    where
        T: Serialize,
    {
        let example = example.into();

        for (_, c) in &mut self.response.content {
            c.example = Some(serde_json::to_value(&example).unwrap());
        }

        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Access the inner [`Response`].
    pub fn inner(&mut self) -> &mut Response {
        self.response
    }
}

/// A transform helper that wraps a callback [`PathItem`].
#[must_use]
pub struct TransformCallback<'t> {
    hidden: bool,
    path: &'t mut PathItem,
}

impl<'t> TransformCallback<'t> {
    /// Create a new transform helper.
    pub fn new(path: &'t mut PathItem) -> Self {
        Self {
            hidden: false,
            path,
        }
    }

    /// Hide the callback from the documentation.
    ///
    /// This is taken into account by generators provided
    /// by this library.
    ///
    /// Hiding an item causes it to be ignored
    /// completely, there is no way to restore or "unhide" it afterwards.
    pub fn hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }

    /// Add a "delete" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn delete(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.delete {
            Some(op) => op,
            None => {
                self.path.delete = Some(Operation::default());
                self.path.delete.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.delete = None;
        }

        self
    }

    /// Add a "get" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn get(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.get {
            Some(op) => op,
            None => {
                self.path.get = Some(Operation::default());
                self.path.get.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.get = None;
        }

        self
    }

    /// Add a "head" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn head(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.head {
            Some(op) => op,
            None => {
                self.path.head = Some(Operation::default());
                self.path.head.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.head = None;
        }

        self
    }

    /// Add a "options" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn options(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.options {
            Some(op) => op,
            None => {
                self.path.options = Some(Operation::default());
                self.path.options.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.options = None;
        }

        self
    }

    /// Add a "patch" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn patch(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.patch {
            Some(op) => op,
            None => {
                self.path.patch = Some(Operation::default());
                self.path.patch.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.patch = None;
        }

        self
    }

    /// Add a "post" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn post(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.post {
            Some(op) => op,
            None => {
                self.path.post = Some(Operation::default());
                self.path.post.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.post = None;
        }

        self
    }

    /// Add a "put" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn put(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.put {
            Some(op) => op,
            None => {
                self.path.put = Some(Operation::default());
                self.path.put.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.put = None;
        }

        self
    }

    /// Add a "trace" callback operation.
    #[allow(clippy::missing_panics_doc)]
    pub fn trace(
        self,
        operation: impl FnOnce(TransformOperation<'_>) -> TransformOperation<'_>,
    ) -> Self {
        let op = match &mut self.path.trace {
            Some(op) => op,
            None => {
                self.path.trace = Some(Operation::default());
                self.path.trace.as_mut().unwrap()
            }
        };

        let t = operation(TransformOperation::new(op));

        if t.hidden {
            self.path.trace = None;
        }

        self
    }

    /// Apply an another transform function.
    pub fn path(
        mut self,
        transform: impl FnOnce(TransformPathItem<'_>) -> TransformPathItem<'_>,
    ) -> Self {
        let t = transform(TransformPathItem::new(self.path));

        if t.hidden {
            self.hidden = true;
        }

        self
    }

    /// Apply an another transform function.
    pub fn with(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }
}

fn filter_no_duplicate_response(err: &Error) -> bool {
    !matches!(err, Error::DefaultResponseExists | Error::ResponseExists(_))
}

pub(crate) fn strip_null_from_query_params_impl(api: &mut OpenApi) {
    let Some(paths) = &mut api.paths else { return };

    for (_, path_item) in &mut paths.paths {
        let ReferenceOr::Item(path_item) = path_item else {
            continue;
        };

        for (_, op) in iter_operations_mut(path_item) {
            for param in &mut op.parameters {
                let ReferenceOr::Item(Parameter::Query { parameter_data, .. }) = param else {
                    continue;
                };

                let ParameterSchemaOrContent::Schema(schema_obj) = &mut parameter_data.format
                else {
                    continue;
                };

                strip_null_from_type(&mut schema_obj.json_schema);
            }
        }
    }
}

fn strip_null_from_type(schema: &mut schemars::Schema) {
    // Handle type: ["string", "null"] -> type: "string"
    if let Some(Value::Array(types)) = schema.get_mut("type") {
        let null_count = types.iter().filter(|t| *t == "null").count();
        if null_count == 0 || null_count == types.len() {
            return; // No nulls, or all nulls - don't modify
        }
        types.retain(|t| t != "null");
        if types.len() == 1 {
            *schema.get_mut("type").unwrap() = types.remove(0);
        }
        return;
    }

    // Handle anyOf: [..., {type: "null"}] -> remove null variants
    let Some(Value::Array(items)) = schema.get_mut("anyOf") else {
        return;
    };

    let is_null = |v: &Value| matches!(v.get("type"), Some(Value::String(s)) if s == "null");
    let null_count = items.iter().filter(|item| is_null(item)).count();
    if null_count == 0 || null_count == items.len() {
        return; // No nulls, or all nulls - don't modify
    }

    items.retain(|item| !is_null(item));

    // Single item remains: unwrap the anyOf
    if items.len() == 1 {
        if let Some(Value::Object(obj)) = items.pop() {
            if let Some(schema_obj) = schema.as_object_mut() {
                schema_obj.remove("anyOf");
                for (key, value) in obj {
                    schema_obj.insert(key, value);
                }
            }
        }
    }
}

/// Reconcile the input (deserialize) and output (serialize) schema definitions produced by the two
/// generators into a single `#/components/schemas` namespace.
///
/// A type whose serialize and deserialize forms are structurally identical — transitively, i.e.
/// including every schema it references — is emitted once under its bare name and shared by
/// requests and responses. A type that diverges is emitted twice: the request keeps the bare name
/// and the response gets a [`OUTPUT_SCHEMA_SUFFIX`](crate::generate)-suffixed copy. Every `$ref` in
/// the document and in the emitted definitions is rewritten to its final name.
pub(crate) fn reconcile_contracts(
    api: &mut OpenApi,
    input_defs: serde_json::Map<String, Value>,
    output_defs: serde_json::Map<String, Value>,
    input_name_ids: Vec<(String, String)>,
    output_name_ids: Vec<(String, String)>,
) {
    let in_prefix = "#/components/schemas/".to_owned();
    let out_prefix = format!("#{}/", crate::generate::OUTPUT_DEFS_PATH);

    let in_name_to_id: BTreeMap<String, String> = input_name_ids.into_iter().collect();
    let out_name_to_id: BTreeMap<String, String> = output_name_ids.into_iter().collect();

    // Index every definition by schema id (the sound join key). Fall back to the name as a
    // synthetic id if the generator did not report one (defensive — should not happen).
    let id_for = |name: &str, table: &BTreeMap<String, String>| {
        table.get(name).cloned().unwrap_or_else(|| name.to_owned())
    };
    let mut in_body: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (name, body) in input_defs {
        let id = id_for(&name, &in_name_to_id);
        in_body.insert(id, (name, body));
    }
    let mut out_body: BTreeMap<String, (String, Value)> = BTreeMap::new();
    for (name, body) in output_defs {
        let id = id_for(&name, &out_name_to_id);
        out_body.insert(id, (name, body));
    }

    // Map a definition's encoded ref tail back to its schema id, per side.
    let in_tail_to_id: BTreeMap<String, String> = in_body
        .iter()
        .map(|(id, (name, _))| (encode_ref_name(name).into_owned(), id.clone()))
        .collect();
    let out_tail_to_id: BTreeMap<String, String> = out_body
        .iter()
        .map(|(id, (name, _))| (encode_ref_name(name).into_owned(), id.clone()))
        .collect();

    // Resolve any `$ref` string to the schema id it targets (`None` if not a components ref). The
    // prefix tells us which side produced it, which is why the two generators emit under disjoint
    // paths.
    let ref_to_id = |r: &str| -> Option<String> {
        if let Some(tail) = r.strip_prefix(in_prefix.as_str()) {
            in_tail_to_id.get(tail).cloned()
        } else if let Some(tail) = r.strip_prefix(out_prefix.as_str()) {
            out_tail_to_id.get(tail).cloned()
        } else {
            None
        }
    };

    // Union ref-graph over ids, and its reverse.
    let all_ids: BTreeSet<String> = in_body.keys().chain(out_body.keys()).cloned().collect();
    let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for id in &all_ids {
        let mut kids = BTreeSet::new();
        if let Some((_, b)) = in_body.get(id) {
            collect_ref_ids(b, &ref_to_id, &mut kids);
        }
        if let Some((_, b)) = out_body.get(id) {
            collect_ref_ids(b, &ref_to_id, &mut kids);
        }
        children.insert(id.clone(), kids);
    }
    let mut preds: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, kids) in &children {
        for k in kids {
            preds.entry(k.clone()).or_default().push(id.clone());
        }
    }

    // Greatest fixpoint: a node is shareable iff its serialize and deserialize forms are locally
    // equal AND every node it references is shareable. Seed with the locally-equal nodes, then
    // evict any node poisoned by a non-shareable child, re-examining its predecessors. Monotone
    // shrink ⇒ terminates; an identical cycle with no path to a divergent node stays shareable.
    let local_equal = |id: &str| match (in_body.get(id), out_body.get(id)) {
        (Some((_, a)), Some((_, b))) => canonicalize(a, &ref_to_id) == canonicalize(b, &ref_to_id),
        _ => false,
    };
    let mut shareable: BTreeSet<String> = all_ids
        .iter()
        .filter(|id| local_equal(id))
        .cloned()
        .collect();
    let mut worklist: Vec<String> = shareable.iter().cloned().collect();
    while let Some(id) = worklist.pop() {
        if !shareable.contains(&id) {
            continue;
        }
        let poisoned = children[&id].iter().any(|c| !shareable.contains(c));
        if poisoned {
            shareable.remove(&id);
            if let Some(ps) = preds.get(&id) {
                worklist.extend(ps.iter().cloned());
            }
        }
    }

    // Assign final names and collect the bodies to emit.
    let mut final_in_name: BTreeMap<String, String> = BTreeMap::new();
    let mut final_out_name: BTreeMap<String, String> = BTreeMap::new();
    let mut taken: BTreeSet<String> = BTreeSet::new();
    let mut emitted: Vec<(String, Value)> = Vec::new();
    let fresh = |base: &str, taken: &mut BTreeSet<String>| -> String {
        if taken.insert(base.to_owned()) {
            return base.to_owned();
        }
        for i in 2.. {
            let cand = format!("{base}{i}");
            if taken.insert(cand.clone()) {
                return cand;
            }
        }
        unreachable!()
    };

    for id in &all_ids {
        let in_present = in_body.contains_key(id);
        let out_present = out_body.contains_key(id);
        let base = in_body
            .get(id)
            .or_else(|| out_body.get(id))
            .map(|(n, _)| n.clone())
            .expect("id came from one of the two maps");

        if shareable.contains(id) {
            let name = fresh(&base, &mut taken);
            let body = in_body
                .get(id)
                .or_else(|| out_body.get(id))
                .map(|(_, b)| b.clone())
                .expect("id came from one of the two maps");
            emitted.push((name.clone(), body));
            final_in_name.insert(id.clone(), name.clone());
            final_out_name.insert(id.clone(), name);
        } else if in_present && out_present {
            let name_in = fresh(&base, &mut taken);
            let name_out = fresh(
                &format!("{base}{}", crate::generate::OUTPUT_SCHEMA_SUFFIX),
                &mut taken,
            );
            emitted.push((name_in.clone(), in_body[id].1.clone()));
            emitted.push((name_out.clone(), out_body[id].1.clone()));
            final_in_name.insert(id.clone(), name_in);
            final_out_name.insert(id.clone(), name_out);
        } else if in_present {
            let name = fresh(&base, &mut taken);
            emitted.push((name.clone(), in_body[id].1.clone()));
            final_in_name.insert(id.clone(), name.clone());
            final_out_name.insert(id.clone(), name);
        } else {
            let name = fresh(&base, &mut taken);
            emitted.push((name.clone(), out_body[id].1.clone()));
            final_in_name.insert(id.clone(), name.clone());
            final_out_name.insert(id.clone(), name);
        }
    }

    let rewriter = Rewriter {
        in_prefix: &in_prefix,
        out_prefix: &out_prefix,
        in_tail_to_id: &in_tail_to_id,
        out_tail_to_id: &out_tail_to_id,
        final_in_name: &final_in_name,
        final_out_name: &final_out_name,
    };

    // Rewrite refs inside the emitted bodies, then insert into components/schemas (sorted by final
    // name for a stable, readable list).
    emitted.sort_by(|a, b| a.0.cmp(&b.0));
    let components = api.components.get_or_insert_with(Default::default);
    for (name, mut body) in emitted {
        rewriter.walk_value(&mut body);
        components.schemas.insert(
            name,
            SchemaObject {
                json_schema: body.try_into().expect("invalid schema"),
                example: None,
                external_docs: None,
            },
        );
    }

    // Rewrite refs throughout the document (operations, parameters, responses, callbacks).
    if let Some(paths) = &mut api.paths {
        for (_, path_item) in &mut paths.paths {
            if let ReferenceOr::Item(path_item) = path_item {
                rewriter.walk_path_item(path_item);
            }
        }
    }

    debug_assert!(
        serde_json::to_string(&*api).map_or(true, |s| !s.contains(&out_prefix)),
        "temporary output schema prefix leaked into the final document"
    );
}

/// Recursively collect the schema ids referenced by `$ref`s anywhere within `v`.
fn collect_ref_ids(
    v: &Value,
    ref_to_id: &impl Fn(&str) -> Option<String>,
    out: &mut BTreeSet<String>,
) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(r)) = m.get("$ref") {
                if let Some(id) = ref_to_id(r) {
                    out.insert(id);
                }
            }
            for (_, child) in m {
                collect_ref_ids(child, ref_to_id, out);
            }
        }
        Value::Array(a) => {
            for child in a {
                collect_ref_ids(child, ref_to_id, out);
            }
        }
        _ => {}
    }
}

/// Produce a comparison-stable clone of `v`: each `$ref` target is replaced by the neutral schema
/// id it points at (so two contracts that reference the same type compare equal regardless of the
/// name that type currently carries), and `required` arrays are sorted (their order is not
/// semantic). Object key order is left as-is — [`serde_json::Map`] equality ignores it.
fn canonicalize(v: &Value, ref_to_id: &impl Fn(&str) -> Option<String>) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, child) in m {
                if k == "$ref" {
                    if let Value::String(r) = child {
                        if let Some(id) = ref_to_id(r) {
                            out.insert(k.clone(), Value::String(format!("\0id:{id}")));
                            continue;
                        }
                    }
                }
                if k == "required" {
                    if let Value::Array(items) = child {
                        let mut canon: Vec<Value> =
                            items.iter().map(|i| canonicalize(i, ref_to_id)).collect();
                        canon.sort_by_key(Value::to_string);
                        out.insert(k.clone(), Value::Array(canon));
                        continue;
                    }
                }
                out.insert(k.clone(), canonicalize(child, ref_to_id));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|c| canonicalize(c, ref_to_id)).collect()),
        _ => v.clone(),
    }
}

/// Encodes a name for insertion into a JSON Pointer URI fragment. Mirrors
/// `schemars::encoding::encode_ref_name` exactly so the tails we build match the ones schemars
/// emits into `$ref`s.
fn encode_ref_name(name: &str) -> std::borrow::Cow<'_, str> {
    fn needs_encoding(byte: u8) -> bool {
        match byte {
            b'~' | b'/' => true,
            b'!' | b'$' | b'&'..=b';' | b'=' | b'?'..=b'Z' | b'_' | b'a'..=b'z' => false,
            _ => true,
        }
    }

    if name.bytes().any(needs_encoding) {
        use std::fmt::Write as _;
        let mut buf = String::new();
        for byte in name.bytes() {
            if byte == b'~' {
                buf.push_str("~0");
            } else if byte == b'/' {
                buf.push_str("~1");
            } else if needs_encoding(byte) {
                write!(buf, "%{byte:2X}").unwrap();
            } else {
                buf.push(byte as char);
            }
        }
        std::borrow::Cow::Owned(buf)
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Rewrites `$ref` strings throughout a document/definition body to their final
/// `#/components/schemas/...` names, routing each ref by its temporary prefix.
struct Rewriter<'a> {
    in_prefix: &'a str,
    out_prefix: &'a str,
    in_tail_to_id: &'a BTreeMap<String, String>,
    out_tail_to_id: &'a BTreeMap<String, String>,
    final_in_name: &'a BTreeMap<String, String>,
    final_out_name: &'a BTreeMap<String, String>,
}

impl Rewriter<'_> {
    fn rewrite_ref(&self, r: &mut String) {
        let (tail_to_id, final_name, tail) = if let Some(tail) = r.strip_prefix(self.in_prefix) {
            (self.in_tail_to_id, self.final_in_name, tail)
        } else if let Some(tail) = r.strip_prefix(self.out_prefix) {
            (self.out_tail_to_id, self.final_out_name, tail)
        } else {
            return;
        };
        if let Some(id) = tail_to_id.get(tail) {
            if let Some(name) = final_name.get(id) {
                *r = format!("{}{}", self.in_prefix, encode_ref_name(name));
            }
        }
    }

    fn walk_value(&self, v: &mut Value) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(s)) = m.get_mut("$ref") {
                    self.rewrite_ref(s);
                }
                for (_, child) in m.iter_mut() {
                    self.walk_value(child);
                }
            }
            Value::Array(a) => {
                for child in a.iter_mut() {
                    self.walk_value(child);
                }
            }
            _ => {}
        }
    }

    fn walk_schema(&self, so: &mut SchemaObject) {
        let mut v = so.json_schema.as_value().clone();
        self.walk_value(&mut v);
        so.json_schema = v.try_into().expect("invalid schema");
    }

    fn walk_media_map(&self, content: &mut IndexMap<String, MediaType>) {
        for (_, mt) in content.iter_mut() {
            if let Some(so) = &mut mt.schema {
                self.walk_schema(so);
            }
        }
    }

    fn walk_param_format(&self, format: &mut ParameterSchemaOrContent) {
        match format {
            ParameterSchemaOrContent::Schema(so) => self.walk_schema(so),
            ParameterSchemaOrContent::Content(content) => self.walk_media_map(content),
        }
    }

    fn walk_parameter(&self, param: &mut Parameter) {
        let data = match param {
            Parameter::Query { parameter_data, .. }
            | Parameter::Header { parameter_data, .. }
            | Parameter::Path { parameter_data, .. }
            | Parameter::Cookie { parameter_data, .. } => parameter_data,
        };
        self.walk_param_format(&mut data.format);
    }

    fn walk_response(&self, response: &mut ReferenceOr<Response>) {
        if let ReferenceOr::Item(resp) = response {
            self.walk_media_map(&mut resp.content);
            for (_, header) in &mut resp.headers {
                if let ReferenceOr::Item(header) = header {
                    self.walk_param_format(&mut header.format);
                }
            }
        }
    }

    fn walk_operation(&self, op: &mut Operation) {
        if let Some(ReferenceOr::Item(rb)) = &mut op.request_body {
            self.walk_media_map(&mut rb.content);
        }
        for param in &mut op.parameters {
            if let ReferenceOr::Item(param) = param {
                self.walk_parameter(param);
            }
        }
        if let Some(responses) = &mut op.responses {
            if let Some(default) = &mut responses.default {
                self.walk_response(default);
            }
            for (_, response) in &mut responses.responses {
                self.walk_response(response);
            }
        }
        for (_, callback) in &mut op.callbacks {
            if let ReferenceOr::Item(callback) = callback {
                for (_, path_item) in callback.iter_mut() {
                    if let ReferenceOr::Item(path_item) = path_item {
                        self.walk_path_item(path_item);
                    }
                }
            }
        }
    }

    fn walk_path_item(&self, path_item: &mut PathItem) {
        for param in &mut path_item.parameters {
            if let ReferenceOr::Item(param) = param {
                self.walk_parameter(param);
            }
        }
        for op in [
            &mut path_item.get,
            &mut path_item.put,
            &mut path_item.post,
            &mut path_item.delete,
            &mut path_item.options,
            &mut path_item.head,
            &mut path_item.patch,
            &mut path_item.trace,
        ]
        .into_iter()
        .flatten()
        {
            self.walk_operation(op);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::openapi::{
        MediaType, OpenApi, Operation, Parameter, ParameterData, ParameterSchemaOrContent, Paths,
        ReferenceOr, RequestBody, SchemaObject,
    };
    use indexmap::IndexMap;
    use schemars::JsonSchema;
    use serde_json::json;

    use super::TransformOpenApi;

    fn inline_schema_for<T: JsonSchema>() -> schemars::Schema {
        let settings = schemars::generate::SchemaSettings::draft07().with(|s| {
            s.inline_subschemas = true;
        });
        let mut gen = settings.into_generator();
        gen.subschema_for::<T>()
    }

    fn property_schema(struct_schema: &schemars::Schema, name: &str) -> schemars::Schema {
        struct_schema
            .get("properties")
            .and_then(|p| p.get(name))
            .unwrap_or_else(|| panic!("property {name:?} not found"))
            .clone()
            .try_into()
            .unwrap()
    }

    fn build_api(params: Vec<Parameter>, body_schema: Option<schemars::Schema>) -> OpenApi {
        let parameters = params.into_iter().map(ReferenceOr::Item).collect();

        let request_body = body_schema.map(|schema| {
            ReferenceOr::Item(RequestBody {
                content: IndexMap::from_iter([(
                    "application/json".into(),
                    MediaType {
                        schema: Some(SchemaObject {
                            json_schema: schema,
                            external_docs: None,
                            example: None,
                        }),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            })
        });

        let op = Operation {
            parameters,
            request_body,
            ..Default::default()
        };

        let path_item = crate::openapi::PathItem {
            get: Some(op),
            ..Default::default()
        };

        OpenApi {
            paths: Some(Paths {
                paths: IndexMap::from([("/test".to_string(), ReferenceOr::Item(path_item))]),
                extensions: IndexMap::new(),
            }),
            ..OpenApi::default()
        }
    }

    fn query_param(name: &str, schema: schemars::Schema) -> Parameter {
        Parameter::Query {
            parameter_data: ParameterData {
                name: name.to_string(),
                description: None,
                required: false,
                deprecated: None,
                format: ParameterSchemaOrContent::Schema(SchemaObject {
                    json_schema: schema,
                    external_docs: None,
                    example: None,
                }),
                example: None,
                examples: IndexMap::new(),
                explode: None,
                extensions: IndexMap::new(),
            },
            allow_reserved: false,
            style: Default::default(),
            allow_empty_value: None,
        }
    }

    fn get_param_schema(api: &OpenApi, param_index: usize) -> &schemars::Schema {
        let paths = api.paths.as_ref().unwrap();
        let ReferenceOr::Item(path_item) = &paths.paths["/test"] else {
            panic!("expected item");
        };
        let op = path_item.get.as_ref().unwrap();
        let ReferenceOr::Item(param) = &op.parameters[param_index] else {
            panic!("expected parameter item");
        };
        let ParameterSchemaOrContent::Schema(schema_obj) = &param.parameter_data_ref().format
        else {
            panic!("expected schema");
        };
        &schema_obj.json_schema
    }

    fn get_body_schema(api: &OpenApi) -> &schemars::Schema {
        let paths = api.paths.as_ref().unwrap();
        let ReferenceOr::Item(path_item) = &paths.paths["/test"] else {
            panic!("expected item");
        };
        let op = path_item.get.as_ref().unwrap();
        let Some(ReferenceOr::Item(body)) = &op.request_body else {
            panic!("expected request body");
        };
        &body.content["application/json"]
            .schema
            .as_ref()
            .unwrap()
            .json_schema
    }

    #[test]
    fn strip_null_from_query_params() {
        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct QueryParams {
            optional: Option<String>,
        }

        #[derive(JsonSchema)]
        #[allow(dead_code)]
        struct Body {
            optional: Option<String>,
        }

        let query_field = property_schema(&inline_schema_for::<QueryParams>(), "optional");
        let body_field = property_schema(&inline_schema_for::<Body>(), "optional");

        // Both should start out nullable
        assert_eq!(query_field.get("type"), Some(&json!(["string", "null"])));
        assert_eq!(body_field.get("type"), Some(&json!(["string", "null"])));

        let mut api = build_api(vec![query_param("optional", query_field)], Some(body_field));
        let _ = TransformOpenApi::new(&mut api).strip_null_from_query_params();

        // Query param should have null stripped
        assert_eq!(
            get_param_schema(&api, 0).get("type"),
            Some(&json!("string"))
        );

        // Request body schema should be unchanged
        assert_eq!(
            get_body_schema(&api).get("type"),
            Some(&json!(["string", "null"])),
            "request body schema should retain its nullable type"
        );
    }
}
