//! Stable `?` types, matching their standard-library branch and conversion rules.

use std::convert::Infallible;
use std::ops::ControlFlow;
use std::task::Poll;

/// Splits a `?` operand into its continued value or residual.
#[doc(hidden)]
pub trait InlineTry {
    /// Value produced when the expression continues.
    type Output;
    /// Value converted into the enclosing function's return value.
    type Residual;
    /// The standard-library `?` branch for this stable type.
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output>;
}

/// Converts a residual into the enclosing function's return value.
#[doc(hidden)]
pub trait InlineResidual<R> {
    /// Applies the standard-library `?` conversion.
    fn from_residual(residual: R) -> Self;
}

impl<T, E> InlineTry for Result<T, E> {
    type Output = T;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, T> {
        match self {
            Ok(value) => ControlFlow::Continue(value),
            Err(error) => ControlFlow::Break(Err(error)),
        }
    }
}

impl<T, E, F: From<E>> InlineResidual<Result<Infallible, E>> for Result<T, F> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        match residual {
            Err(error) => Err(From::from(error)),
            Ok(value) => match value {},
        }
    }
}

impl<T> InlineTry for Option<T> {
    type Output = T;
    type Residual = Option<Infallible>;
    fn branch(self) -> ControlFlow<Self::Residual, T> {
        match self {
            Some(value) => ControlFlow::Continue(value),
            None => ControlFlow::Break(None),
        }
    }
}

impl<T> InlineResidual<Option<Infallible>> for Option<T> {
    fn from_residual(residual: Option<Infallible>) -> Self {
        match residual {
            None => None,
            Some(value) => match value {},
        }
    }
}

impl<B, C> InlineTry for ControlFlow<B, C> {
    type Output = C;
    type Residual = ControlFlow<B, Infallible>;
    fn branch(self) -> ControlFlow<Self::Residual, C> {
        match self {
            ControlFlow::Continue(value) => ControlFlow::Continue(value),
            ControlFlow::Break(value) => ControlFlow::Break(ControlFlow::Break(value)),
        }
    }
}

impl<B, C> InlineResidual<ControlFlow<B, Infallible>> for ControlFlow<B, C> {
    fn from_residual(residual: ControlFlow<B, Infallible>) -> Self {
        match residual {
            ControlFlow::Break(value) => ControlFlow::Break(value),
            ControlFlow::Continue(value) => match value {},
        }
    }
}

impl<T, E> InlineTry for Poll<Result<T, E>> {
    type Output = Poll<T>;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Poll::Ready(Ok(value)) => ControlFlow::Continue(Poll::Ready(value)),
            Poll::Ready(Err(error)) => ControlFlow::Break(Err(error)),
            Poll::Pending => ControlFlow::Continue(Poll::Pending),
        }
    }
}

impl<T, E, F: From<E>> InlineResidual<Result<Infallible, E>> for Poll<Result<T, F>> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        Poll::Ready(InlineResidual::from_residual(residual))
    }
}

impl<T, E> InlineTry for Poll<Option<Result<T, E>>> {
    type Output = Poll<Option<T>>;
    type Residual = Result<Infallible, E>;
    fn branch(self) -> ControlFlow<Self::Residual, Self::Output> {
        match self {
            Poll::Ready(Some(Ok(value))) => ControlFlow::Continue(Poll::Ready(Some(value))),
            Poll::Ready(Some(Err(error))) => ControlFlow::Break(Err(error)),
            Poll::Ready(None) => ControlFlow::Continue(Poll::Ready(None)),
            Poll::Pending => ControlFlow::Continue(Poll::Pending),
        }
    }
}

impl<T, E, F: From<E>> InlineResidual<Result<Infallible, E>> for Poll<Option<Result<T, F>>> {
    fn from_residual(residual: Result<Infallible, E>) -> Self {
        Poll::Ready(Some(InlineResidual::from_residual(residual)))
    }
}
