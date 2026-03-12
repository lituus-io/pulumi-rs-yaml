use crate::ast::interpolation::InterpolationPart;
use crate::ast::property::PropertyAccess;
use crate::syntax::ExprMeta;
use std::borrow::Cow;

/// The core expression AST for Pulumi YAML.
///
/// All 25+ expression variants are represented as a single enum with no dynamic dispatch.
/// Each variant carries an `ExprMeta` for source location tracking.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr<'src> {
    /// Null literal.
    Null(ExprMeta),
    /// Boolean literal.
    Bool(ExprMeta, bool),
    /// Number literal (f64).
    Number(ExprMeta, f64),
    /// String literal (may be borrowed from source or owned).
    String(ExprMeta, Cow<'src, str>),
    /// Interpolated string containing `${...}` expressions.
    Interpolate(ExprMeta, Vec<InterpolationPart<'src>>),
    /// Symbol reference: a bare `${resource.property}`.
    Symbol(ExprMeta, PropertyAccess<'src>),
    /// List of expressions.
    List(ExprMeta, Vec<Expr<'src>>),
    /// Object with key-value pairs.
    Object(ExprMeta, Vec<ObjectProperty<'src>>),

    // --- Builtin functions ---
    /// `fn::invoke` - invokes a Pulumi function.
    Invoke(ExprMeta, InvokeExpr<'src>),
    /// `fn::join` - joins a list with a delimiter.
    Join(ExprMeta, Box<Expr<'src>>, Box<Expr<'src>>),
    /// `fn::select` - selects an element from a list by index.
    Select(ExprMeta, Box<Expr<'src>>, Box<Expr<'src>>),
    /// `fn::split` - splits a string by a delimiter.
    Split(ExprMeta, Box<Expr<'src>>, Box<Expr<'src>>),
    /// `fn::toJSON` - serializes a value to JSON.
    ToJson(ExprMeta, Box<Expr<'src>>),
    /// `fn::toBase64` - encodes a string as base64.
    ToBase64(ExprMeta, Box<Expr<'src>>),
    /// `fn::fromBase64` - decodes a base64 string.
    FromBase64(ExprMeta, Box<Expr<'src>>),
    /// `fn::secret` - marks a value as secret.
    Secret(ExprMeta, Box<Expr<'src>>),
    /// `fn::readFile` - reads a file at the given path.
    ReadFile(ExprMeta, Box<Expr<'src>>),

    // --- Math builtins ---
    /// `fn::abs` - absolute value of a number.
    Abs(ExprMeta, Box<Expr<'src>>),
    /// `fn::floor` - floor of a number.
    Floor(ExprMeta, Box<Expr<'src>>),
    /// `fn::ceil` - ceiling of a number.
    Ceil(ExprMeta, Box<Expr<'src>>),
    /// `fn::max` - maximum value in a list of numbers.
    Max(ExprMeta, Box<Expr<'src>>),
    /// `fn::min` - minimum value in a list of numbers.
    Min(ExprMeta, Box<Expr<'src>>),

    // --- String builtins ---
    /// `fn::stringLen` - length of a string (Unicode char count).
    StringLen(ExprMeta, Box<Expr<'src>>),
    /// `fn::substring` - extracts a substring: [source, start, length].
    Substring(ExprMeta, Box<Expr<'src>>, Box<Expr<'src>>, Box<Expr<'src>>),

    // --- Time builtins ---
    /// `fn::timeUtc` - current UTC time as ISO 8601 string.
    TimeUtc(ExprMeta, Box<Expr<'src>>),
    /// `fn::timeUnix` - current Unix timestamp as a number.
    TimeUnix(ExprMeta, Box<Expr<'src>>),

    // --- UUID/Random builtins ---
    /// `fn::uuid` - generates a random UUID v4.
    Uuid(ExprMeta, Box<Expr<'src>>),
    /// `fn::randomString` - generates a random alphanumeric string of given length.
    RandomString(ExprMeta, Box<Expr<'src>>),

    // --- Date builtins ---
    /// `fn::dateFormat` - formats the current date/time with a strftime-style format string.
    DateFormat(ExprMeta, Box<Expr<'src>>),

    // --- Assets and archives ---
    /// `fn::stringAsset` - creates an asset from a string.
    StringAsset(ExprMeta, Box<Expr<'src>>),
    /// `fn::fileAsset` - creates an asset from a file path.
    FileAsset(ExprMeta, Box<Expr<'src>>),
    /// `fn::remoteAsset` - creates an asset from a URL.
    RemoteAsset(ExprMeta, Box<Expr<'src>>),
    /// `fn::fileArchive` - creates an archive from a file path.
    FileArchive(ExprMeta, Box<Expr<'src>>),
    /// `fn::remoteArchive` - creates an archive from a URL.
    RemoteArchive(ExprMeta, Box<Expr<'src>>),
    /// `fn::assetArchive` - creates an archive from a map of assets/archives.
    AssetArchive(ExprMeta, Vec<(Cow<'src, str>, Expr<'src>)>),
}

/// An object property: a key-value pair where the key is an expression (typically a string).
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectProperty<'src> {
    pub key: Box<Expr<'src>>,
    pub value: Box<Expr<'src>>,
}

/// Arguments for `fn::invoke`.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeExpr<'src> {
    /// The function token (e.g. `aws:s3:getBucket`).
    pub token: Cow<'src, str>,
    /// The function arguments (an object expression, or None).
    pub call_args: Option<Box<Expr<'src>>>,
    /// Invoke options.
    pub call_opts: InvokeOptions<'src>,
    /// Return directive (specific output property name).
    pub return_: Option<Cow<'src, str>>,
}

/// Options for `fn::invoke`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InvokeOptions<'src> {
    pub parent: Option<Box<Expr<'src>>>,
    pub provider: Option<Box<Expr<'src>>>,
    pub depends_on: Option<Box<Expr<'src>>>,
    pub version: Option<Cow<'src, str>>,
    pub plugin_download_url: Option<Cow<'src, str>>,
}

impl Expr<'_> {
    /// Returns the metadata (span info) for this expression.
    pub fn meta(&self) -> &ExprMeta {
        match self {
            Expr::Null(m)
            | Expr::Bool(m, _)
            | Expr::Number(m, _)
            | Expr::String(m, _)
            | Expr::Interpolate(m, _)
            | Expr::Symbol(m, _)
            | Expr::List(m, _)
            | Expr::Object(m, _)
            | Expr::Invoke(m, _)
            | Expr::Join(m, _, _)
            | Expr::Select(m, _, _)
            | Expr::Split(m, _, _)
            | Expr::ToJson(m, _)
            | Expr::ToBase64(m, _)
            | Expr::FromBase64(m, _)
            | Expr::Secret(m, _)
            | Expr::ReadFile(m, _)
            | Expr::Abs(m, _)
            | Expr::Floor(m, _)
            | Expr::Ceil(m, _)
            | Expr::Max(m, _)
            | Expr::Min(m, _)
            | Expr::StringLen(m, _)
            | Expr::TimeUtc(m, _)
            | Expr::TimeUnix(m, _)
            | Expr::Uuid(m, _)
            | Expr::RandomString(m, _)
            | Expr::DateFormat(m, _)
            | Expr::StringAsset(m, _)
            | Expr::FileAsset(m, _)
            | Expr::RemoteAsset(m, _)
            | Expr::FileArchive(m, _)
            | Expr::RemoteArchive(m, _)
            | Expr::AssetArchive(m, _) => m,
            Expr::Substring(m, _, _, _) => m,
        }
    }

    /// Returns true if this expression is a string literal.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Expr::String(_, s) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// Returns true if this is a symbol expression.
    pub fn is_symbol(&self) -> bool {
        matches!(self, Expr::Symbol(_, _))
    }

    /// Returns true if this is an asset or archive expression.
    pub fn is_asset_or_archive(&self) -> bool {
        matches!(
            self,
            Expr::StringAsset(_, _)
                | Expr::FileAsset(_, _)
                | Expr::RemoteAsset(_, _)
                | Expr::FileArchive(_, _)
                | Expr::RemoteArchive(_, _)
                | Expr::AssetArchive(_, _)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expr_meta_access() {
        let expr = Expr::Null(ExprMeta::no_span());
        assert!(expr.meta().span.is_none());
    }

    #[test]
    fn test_expr_as_str() {
        let expr = Expr::String(ExprMeta::no_span(), Cow::Borrowed("hello"));
        assert_eq!(expr.as_str(), Some("hello"));
        let expr2 = Expr::Null(ExprMeta::no_span());
        assert_eq!(expr2.as_str(), None);
    }

    #[test]
    fn test_expr_is_symbol() {
        let expr = Expr::Symbol(
            ExprMeta::no_span(),
            PropertyAccess {
                accessors: vec![crate::ast::property::PropertyAccessor::Name(Cow::Borrowed(
                    "res",
                ))],
            },
        );
        assert!(expr.is_symbol());
        assert!(!Expr::Null(ExprMeta::no_span()).is_symbol());
    }

    #[test]
    fn test_is_asset_or_archive() {
        let expr = Expr::FileAsset(
            ExprMeta::no_span(),
            Box::new(Expr::String(ExprMeta::no_span(), Cow::Borrowed("file.txt"))),
        );
        assert!(expr.is_asset_or_archive());
        assert!(!Expr::Null(ExprMeta::no_span()).is_asset_or_archive());
    }

    #[test]
    fn test_invoke_expr() {
        let invoke = InvokeExpr {
            token: Cow::Borrowed("aws:s3:getBucket"),
            call_args: None,
            call_opts: InvokeOptions::default(),
            return_: Some(Cow::Borrowed("arn")),
        };
        let expr = Expr::Invoke(ExprMeta::no_span(), invoke);
        assert!(expr.meta().span.is_none());
    }

    #[test]
    fn test_object_property() {
        let prop = ObjectProperty {
            key: Box::new(Expr::String(ExprMeta::no_span(), Cow::Borrowed("key"))),
            value: Box::new(Expr::Number(ExprMeta::no_span(), 42.0)),
        };
        assert_eq!(prop.key.as_str(), Some("key"));
    }
}
