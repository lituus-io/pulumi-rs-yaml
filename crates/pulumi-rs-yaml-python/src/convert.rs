use std::borrow::Cow;
use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::expr::{Expr, InvokeExpr, InvokeOptions, ObjectProperty};
use pulumi_rs_yaml_core::ast::interpolation::InterpolationPart;
use pulumi_rs_yaml_core::ast::property::{PropertyAccess, PropertyAccessor};
use pulumi_rs_yaml_core::ast::template::{PropertyEntry, ResourceOptionsDecl, ResourceProperties};
use pulumi_rs_yaml_core::eval::value::Value;
use pulumi_rs_yaml_core::packages::canonicalize_type_token;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString};

/// Converts a Rust `Value` to a Python object.
pub fn value_to_py(py: Python<'_>, val: &Value<'_>) -> PyResult<PyObject> {
    match val {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any().unbind()),
        Value::Number(n) => {
            if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
                Ok((*n as i64).into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(n.into_pyobject(py)?.into_any().unbind())
            }
        }
        Value::String(s) => Ok(PyString::new(py, s.as_ref()).into_any().unbind()),
        Value::List(items) => {
            let py_items: Vec<PyObject> = items
                .iter()
                .map(|item| value_to_py(py, item))
                .collect::<PyResult<_>>()?;
            Ok(PyList::new(py, &py_items)?.into_any().unbind())
        }
        Value::Object(entries) => {
            let dict = PyDict::new(py);
            for (k, v) in entries {
                dict.set_item(k.as_ref(), value_to_py(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
        Value::Secret(inner) => {
            let dict = PyDict::new(py);
            dict.set_item("__secret", true)?;
            dict.set_item("value", value_to_py(py, inner)?)?;
            Ok(dict.into_any().unbind())
        }
        Value::Unknown => Ok(py.None()),
        _ => Ok(py.None()),
    }
}

/// Converts a Python object to a Rust `Value<'static>`.
pub fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<Value<'static>> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.downcast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if let Ok(i) = obj.downcast::<PyInt>() {
        let n: i64 = i.extract()?;
        return Ok(Value::Number(n as f64));
    }
    if let Ok(f) = obj.downcast::<PyFloat>() {
        let n: f64 = f.extract()?;
        return Ok(Value::Number(n));
    }
    if let Ok(s) = obj.downcast::<PyString>() {
        let val: String = s.extract()?;
        return Ok(Value::String(Cow::Owned(val)));
    }
    if let Ok(list) = obj.downcast::<PyList>() {
        let items: Vec<Value<'static>> = list
            .iter()
            .map(|item| py_to_value(&item))
            .collect::<PyResult<_>>()?;
        return Ok(Value::List(items));
    }
    if let Ok(dict) = obj.downcast::<PyDict>() {
        let entries: Vec<(Cow<'static, str>, Value<'static>)> = dict
            .iter()
            .map(|(k, v)| {
                let key: String = k.extract()?;
                let val = py_to_value(&v)?;
                Ok((Cow::Owned(key), val))
            })
            .collect::<PyResult<_>>()?;
        return Ok(Value::Object(entries));
    }
    let s: String = obj.str()?.extract()?;
    Ok(Value::String(Cow::Owned(s)))
}

/// Converts a Python dict to `HashMap<String, String>`.
pub fn py_dict_to_string_map(dict: &Bound<'_, PyDict>) -> PyResult<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (k, v) in dict.iter() {
        let key: String = k.extract()?;
        let val: String = v.extract()?;
        map.insert(key, val);
    }
    Ok(map)
}

// =============================================================================
// Expr → Python dict serialization
// =============================================================================

/// Converts a single PropertyAccessor to a Python dict.
fn accessor_to_py(py: Python<'_>, acc: &PropertyAccessor<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    match acc {
        PropertyAccessor::Name(n) => {
            dict.set_item("t", "name")?;
            dict.set_item("v", n.as_ref())?;
        }
        PropertyAccessor::StringSubscript(s) => {
            dict.set_item("t", "str_sub")?;
            dict.set_item("v", s.as_ref())?;
        }
        PropertyAccessor::IntSubscript(i) => {
            dict.set_item("t", "int_sub")?;
            dict.set_item("v", *i)?;
        }
    }
    Ok(dict.into_any().unbind())
}

/// Converts a PropertyAccess chain to a Python list of accessor dicts.
fn access_to_py(py: Python<'_>, access: &PropertyAccess<'_>) -> PyResult<PyObject> {
    let items: Vec<PyObject> = access
        .accessors
        .iter()
        .map(|a| accessor_to_py(py, a))
        .collect::<PyResult<_>>()?;
    Ok(PyList::new(py, &items)?.into_any().unbind())
}

/// Helper: create a single-arg builtin dict `{"t": tag, "arg": expr}`.
fn single_arg_to_py(py: Python<'_>, tag: &str, arg: &Expr<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("t", tag)?;
    dict.set_item("arg", expr_to_py(py, arg)?)?;
    Ok(dict.into_any().unbind())
}

/// Converts an `Expr<'src>` to a Python dict with `"t"` type discriminator.
pub fn expr_to_py(py: Python<'_>, expr: &Expr<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    match expr {
        Expr::Null(_) => {
            dict.set_item("t", "null")?;
            Ok(dict.into_any().unbind())
        }
        Expr::Bool(_, b) => {
            dict.set_item("t", "bool")?;
            dict.set_item("v", *b)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Number(_, n) => {
            dict.set_item("t", "number")?;
            if n.fract() == 0.0 && n.abs() < (i64::MAX as f64) {
                dict.set_item("v", *n as i64)?;
            } else {
                dict.set_item("v", *n)?;
            }
            Ok(dict.into_any().unbind())
        }
        Expr::String(_, s) => {
            dict.set_item("t", "string")?;
            dict.set_item("v", s.as_ref())?;
            Ok(dict.into_any().unbind())
        }
        Expr::Symbol(_, access) => {
            dict.set_item("t", "sym")?;
            dict.set_item("a", access_to_py(py, access)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Interpolate(_, parts) => {
            dict.set_item("t", "interp")?;
            let py_parts: Vec<PyObject> = parts
                .iter()
                .map(|part| interp_part_to_py(py, part))
                .collect::<PyResult<_>>()?;
            dict.set_item("parts", PyList::new(py, &py_parts)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::List(_, items) => {
            dict.set_item("t", "list")?;
            let py_items: Vec<PyObject> = items
                .iter()
                .map(|item| expr_to_py(py, item))
                .collect::<PyResult<_>>()?;
            dict.set_item("items", PyList::new(py, &py_items)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Object(_, entries) => {
            dict.set_item("t", "obj")?;
            let py_entries: Vec<PyObject> = entries
                .iter()
                .map(|e| obj_prop_to_py(py, e))
                .collect::<PyResult<_>>()?;
            dict.set_item("entries", PyList::new(py, &py_entries)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Invoke(_, inv) => invoke_to_py(py, inv),
        Expr::Join(_, sep, vals) => {
            dict.set_item("t", "join")?;
            dict.set_item("sep", expr_to_py(py, sep)?)?;
            dict.set_item("vals", expr_to_py(py, vals)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Select(_, idx, vals) => {
            dict.set_item("t", "select")?;
            dict.set_item("idx", expr_to_py(py, idx)?)?;
            dict.set_item("vals", expr_to_py(py, vals)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Split(_, sep, src) => {
            dict.set_item("t", "split")?;
            dict.set_item("sep", expr_to_py(py, sep)?)?;
            dict.set_item("src", expr_to_py(py, src)?)?;
            Ok(dict.into_any().unbind())
        }
        Expr::Substring(_, src, start, len) => {
            dict.set_item("t", "substring")?;
            dict.set_item("src", expr_to_py(py, src)?)?;
            dict.set_item("start", expr_to_py(py, start)?)?;
            dict.set_item("len", expr_to_py(py, len)?)?;
            Ok(dict.into_any().unbind())
        }
        // Single-arg builtins
        Expr::ToJson(_, a) => single_arg_to_py(py, "toJSON", a),
        Expr::ToBase64(_, a) => single_arg_to_py(py, "toBase64", a),
        Expr::FromBase64(_, a) => single_arg_to_py(py, "fromBase64", a),
        Expr::Secret(_, a) => single_arg_to_py(py, "secret", a),
        Expr::ReadFile(_, a) => single_arg_to_py(py, "readFile", a),
        Expr::Abs(_, a) => single_arg_to_py(py, "abs", a),
        Expr::Floor(_, a) => single_arg_to_py(py, "floor", a),
        Expr::Ceil(_, a) => single_arg_to_py(py, "ceil", a),
        Expr::Max(_, a) => single_arg_to_py(py, "max", a),
        Expr::Min(_, a) => single_arg_to_py(py, "min", a),
        Expr::StringLen(_, a) => single_arg_to_py(py, "stringLen", a),
        Expr::TimeUtc(_, a) => single_arg_to_py(py, "timeUtc", a),
        Expr::TimeUnix(_, a) => single_arg_to_py(py, "timeUnix", a),
        Expr::Uuid(_, a) => single_arg_to_py(py, "uuid", a),
        Expr::RandomString(_, a) => single_arg_to_py(py, "randomString", a),
        Expr::DateFormat(_, a) => single_arg_to_py(py, "dateFormat", a),
        // Assets/Archives
        Expr::StringAsset(_, a) => single_arg_to_py(py, "stringAsset", a),
        Expr::FileAsset(_, a) => single_arg_to_py(py, "fileAsset", a),
        Expr::RemoteAsset(_, a) => single_arg_to_py(py, "remoteAsset", a),
        Expr::FileArchive(_, a) => single_arg_to_py(py, "fileArchive", a),
        Expr::RemoteArchive(_, a) => single_arg_to_py(py, "remoteArchive", a),
        Expr::AssetArchive(_, entries) => {
            dict.set_item("t", "assetArchive")?;
            let py_entries: Vec<PyObject> = entries
                .iter()
                .map(|(k, v)| {
                    let entry = PyDict::new(py);
                    entry.set_item("k", k.as_ref())?;
                    entry.set_item("v", expr_to_py(py, v)?)?;
                    Ok(entry.into_any().unbind())
                })
                .collect::<PyResult<_>>()?;
            dict.set_item("entries", PyList::new(py, &py_entries)?)?;
            Ok(dict.into_any().unbind())
        }
    }
}

/// Converts an InterpolationPart to a Python dict.
fn interp_part_to_py(py: Python<'_>, part: &InterpolationPart<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("text", part.text.as_ref())?;
    if let Some(ref access) = part.value {
        dict.set_item("a", access_to_py(py, access)?)?;
    } else {
        dict.set_item("a", py.None())?;
    }
    Ok(dict.into_any().unbind())
}

/// Converts an ObjectProperty to a Python dict.
fn obj_prop_to_py(py: Python<'_>, prop: &ObjectProperty<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("k", expr_to_py(py, &prop.key)?)?;
    dict.set_item("v", expr_to_py(py, &prop.value)?)?;
    Ok(dict.into_any().unbind())
}

/// Converts an InvokeExpr to a Python dict.
fn invoke_to_py(py: Python<'_>, inv: &InvokeExpr<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("t", "invoke")?;
    let canonical_token = canonicalize_type_token(inv.token.as_ref());
    dict.set_item("tok", canonical_token.as_str())?;
    if let Some(ref args) = inv.call_args {
        dict.set_item("args", expr_to_py(py, args)?)?;
    } else {
        dict.set_item("args", py.None())?;
    }
    if let Some(ref ret) = inv.return_ {
        dict.set_item("ret", ret.as_ref())?;
    } else {
        dict.set_item("ret", py.None())?;
    }
    dict.set_item("opts", invoke_options_to_py(py, &inv.call_opts)?)?;
    Ok(dict.into_any().unbind())
}

/// Converts InvokeOptions to a Python dict.
fn invoke_options_to_py(py: Python<'_>, opts: &InvokeOptions<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    if let Some(ref p) = opts.parent {
        dict.set_item("parent", expr_to_py(py, p)?)?;
    }
    if let Some(ref p) = opts.provider {
        dict.set_item("provider", expr_to_py(py, p)?)?;
    }
    if let Some(ref d) = opts.depends_on {
        dict.set_item("dependsOn", expr_to_py(py, d)?)?;
    }
    if let Some(ref v) = opts.version {
        dict.set_item("version", v.as_ref())?;
    }
    if let Some(ref u) = opts.plugin_download_url {
        dict.set_item("pluginDownloadURL", u.as_ref())?;
    }
    Ok(dict.into_any().unbind())
}

/// Converts ResourceOptionsDecl to a Python dict.
pub fn resource_options_to_py(
    py: Python<'_>,
    opts: &ResourceOptionsDecl<'_>,
) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    if let Some(ref d) = opts.depends_on {
        dict.set_item("dependsOn", expr_to_py(py, d)?)?;
    }
    if let Some(ref p) = opts.parent {
        dict.set_item("parent", expr_to_py(py, p)?)?;
    }
    if let Some(ref p) = opts.provider {
        dict.set_item("provider", expr_to_py(py, p)?)?;
    }
    if let Some(ref p) = opts.providers {
        dict.set_item("providers", expr_to_py(py, p)?)?;
    }
    if let Some(ref a) = opts.aliases {
        dict.set_item("aliases", expr_to_py(py, a)?)?;
    }
    if let Some(ref p) = opts.protect {
        dict.set_item("protect", expr_to_py(py, p)?)?;
    }
    if let Some(b) = opts.delete_before_replace {
        dict.set_item("deleteBeforeReplace", b)?;
    }
    if let Some(ref ic) = opts.ignore_changes {
        let strs: Vec<&str> = ic.iter().map(|s| s.as_ref()).collect();
        dict.set_item("ignoreChanges", strs)?;
    }
    if let Some(ref imp) = opts.import {
        dict.set_item("import", imp.as_ref())?;
    }
    if let Some(ref v) = opts.version {
        dict.set_item("version", v.as_ref())?;
    }
    if let Some(ref u) = opts.plugin_download_url {
        dict.set_item("pluginDownloadURL", u.as_ref())?;
    }
    if let Some(ref aso) = opts.additional_secret_outputs {
        let strs: Vec<&str> = aso.iter().map(|s| s.as_ref()).collect();
        dict.set_item("additionalSecretOutputs", strs)?;
    }
    if let Some(ref ct) = opts.custom_timeouts {
        let ct_dict = PyDict::new(py);
        if let Some(ref c) = ct.create {
            ct_dict.set_item("create", c.as_ref())?;
        }
        if let Some(ref u) = ct.update {
            ct_dict.set_item("update", u.as_ref())?;
        }
        if let Some(ref d) = ct.delete {
            ct_dict.set_item("delete", d.as_ref())?;
        }
        dict.set_item("customTimeouts", ct_dict)?;
    }
    if let Some(ref roc) = opts.replace_on_changes {
        let strs: Vec<&str> = roc.iter().map(|s| s.as_ref()).collect();
        dict.set_item("replaceOnChanges", strs)?;
    }
    if let Some(b) = opts.retain_on_delete {
        dict.set_item("retainOnDelete", b)?;
    }
    if let Some(ref rw) = opts.replace_with {
        dict.set_item("replaceWith", expr_to_py(py, rw)?)?;
    }
    if let Some(ref dw) = opts.deleted_with {
        dict.set_item("deletedWith", expr_to_py(py, dw)?)?;
    }
    if let Some(ref hd) = opts.hide_diffs {
        let strs: Vec<&str> = hd.iter().map(|s| s.as_ref()).collect();
        dict.set_item("hideDiffs", strs)?;
    }
    Ok(dict.into_any().unbind())
}

/// Converts ResourceProperties to a Python object (list of dicts or single expr dict).
pub fn resource_properties_to_py(
    py: Python<'_>,
    props: &ResourceProperties<'_>,
) -> PyResult<PyObject> {
    match props {
        ResourceProperties::Map(entries) => {
            let py_entries: Vec<PyObject> = entries
                .iter()
                .map(|e| property_entry_to_py(py, e))
                .collect::<PyResult<_>>()?;
            Ok(PyList::new(py, &py_entries)?.into_any().unbind())
        }
        ResourceProperties::Expr(expr) => expr_to_py(py, expr),
    }
}

/// Converts a PropertyEntry to a Python dict.
fn property_entry_to_py(py: Python<'_>, entry: &PropertyEntry<'_>) -> PyResult<PyObject> {
    let dict = PyDict::new(py);
    dict.set_item("k", entry.key.as_ref())?;
    dict.set_item("v", expr_to_py(py, &entry.value)?)?;
    Ok(dict.into_any().unbind())
}
