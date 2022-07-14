use std::cmp::Ordering;
use std::os::raw::c_int;
use std::ptr::null_mut;

use pyo3::conversion::AsPyPointer;
use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple, PyType};
use pyo3::{ffi, intern};

use crate::build_tools::{py_error, SchemaDict};
use crate::errors::{ErrorKind, ValError, ValResult};
use crate::input::Input;
use crate::recursion_guard::RecursionGuard;

use super::{build_validator, BuildContext, BuildValidator, CombinedValidator, Extra, Validator};

#[derive(Debug, Clone)]
pub struct ModelClassValidator {
    strict: bool,
    validator: Box<CombinedValidator>,
    class: Py<PyType>,
    name: String,
}

impl BuildValidator for ModelClassValidator {
    const EXPECTED_TYPE: &'static str = "model-class";

    fn build(
        schema: &PyDict,
        config: Option<&PyDict>,
        build_context: &mut BuildContext,
    ) -> PyResult<CombinedValidator> {
        // models ignore the parent config and always use the config from this model
        let config = build_config(schema.py(), schema, config)?;

        let class: &PyType = schema.get_as_req("class_type")?;
        let sub_schema: &PyAny = schema.get_as_req("schema")?;
        let (validator, td_schema) = build_validator(sub_schema, config, build_context)?;
        let schema_type: String = td_schema.get_as_req("type")?;
        if &schema_type != "typed-dict" {
            return py_error!("model-class expected a 'typed-dict' schema, got '{}'", schema_type);
        }
        let return_fields_set = td_schema.get_as("return_fields_set")?.unwrap_or(false);
        if !return_fields_set {
            return py_error!(r#"model-class inner schema must have "return_fields_set" set to True"#);
        }

        Ok(Self {
            // we don't use is_strict here since we don't want validation to be strict in this case if
            // `config.strict` is set, only if this specific field is strict
            strict: schema.get_as("strict")?.unwrap_or(false),
            validator: Box::new(validator),
            class: class.into(),
            // Get the class's `__name__`, not using `class.name()` since it uses `__qualname__`
            // which is not what we want here
            name: class.getattr(intern!(schema.py(), "__name__"))?.extract()?,
        }
        .into())
    }
}

impl Validator for ModelClassValidator {
    fn validate<'s, 'data>(
        &'s self,
        py: Python<'data>,
        input: &'data impl Input<'data>,
        extra: &Extra,
        slots: &'data [CombinedValidator],
        recursion_guard: &'s mut RecursionGuard,
    ) -> ValResult<'data, PyObject> {
        let class = self.class.as_ref(py);
        if input.is_type(class)? {
            Ok(input.to_object(py))
        } else if extra.strict.unwrap_or(self.strict) {
            Err(ValError::new(
                ErrorKind::ModelClassType {
                    class_name: self.get_name().to_string(),
                },
                input,
            ))
        } else {
            let output = self.validator.validate(py, input, extra, slots, recursion_guard)?;
            self.create_class(py, output).map_err(Into::<ValError>::into)
        }
    }

    fn get_name(&self) -> &str {
        &self.name
    }
}

impl ModelClassValidator {
    fn create_class(&self, py: Python, output: PyObject) -> PyResult<PyObject> {
        let (model_dict, fields_set): (&PyAny, &PyAny) = output.extract(py)?;

        // based on the following but with the second argument of new_func set to an empty tuple as required
        // https://github.com/PyO3/pyo3/blob/d2caa056e9aacc46374139ef491d112cb8af1a25/src/pyclass_init.rs#L35-L77
        let args = PyTuple::empty(py);
        let raw_type = self.class.as_ref(py).as_type_ptr();
        let instance = unsafe {
            // Safety: raw_type is known to be a non-null type object pointer
            match (*raw_type).tp_new {
                // Safety: the result of new_func is guaranteed to be either an owned pointer or null on error returns.
                Some(new_func) => PyObject::from_owned_ptr_or_err(
                    py,
                    // Safety: the non-null pointers are known to be valid, and it's allowed to call tp_new with a
                    // null kwargs dict.
                    new_func(raw_type, args.as_ptr(), null_mut()),
                )?,
                None => return Err(PyTypeError::new_err("base type without tp_new")),
            }
        };

        let instance_ref = instance.as_ref(py);
        force_setattr(py, instance_ref, intern!(py, "__dict__"), model_dict)?;
        force_setattr(py, instance_ref, intern!(py, "__fields_set__"), fields_set)?;

        Ok(instance)
    }
}

pub fn force_setattr<N, V>(py: Python<'_>, obj: &PyAny, attr_name: N, value: V) -> PyResult<()>
where
    N: ToPyObject,
    V: ToPyObject,
{
    let attr_name = attr_name.to_object(py);
    let value = value.to_object(py);
    unsafe {
        error_on_minusone(
            py,
            ffi::PyObject_GenericSetAttr(obj.as_ptr(), attr_name.as_ptr(), value.as_ptr()),
        )
    }
}

// Defined here as it's not exported by pyo3
#[inline]
fn error_on_minusone(py: Python<'_>, result: c_int) -> PyResult<()> {
    if result != -1 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

fn build_config<'a>(
    py: Python<'a>,
    schema: &'a PyDict,
    parent_config: Option<&'a PyDict>,
) -> PyResult<Option<&'a PyDict>> {
    let child_config: Option<&PyDict> = schema.get_as("config")?;
    match (parent_config, child_config) {
        (Some(parent), None) => Ok(Some(parent)),
        (None, Some(child)) => Ok(Some(child)),
        (None, None) => Ok(None),
        (Some(parent), Some(child)) => {
            let parent_choose: i32 = parent.get_as("config_choose_priority")?.unwrap_or_default();
            let child_choose: i32 = child.get_as("config_choose_priority")?.unwrap_or_default();
            match parent_choose.cmp(&child_choose) {
                Ordering::Greater => Ok(Some(parent)),
                Ordering::Less => Ok(Some(child)),
                Ordering::Equal => {
                    let parent_merge: i32 = parent.get_as("config_merge_priority")?.unwrap_or_default();
                    let child_merge: i32 = child.get_as("config_merge_priority")?.unwrap_or_default();
                    match parent_merge.cmp(&child_merge) {
                        Ordering::Greater => {
                            child.getattr(intern!(py, "update"))?.call1((parent,))?;
                            Ok(Some(child))
                        }
                        // otherwise child is the winner
                        _ => {
                            parent.getattr(intern!(py, "update"))?.call1((child,))?;
                            Ok(Some(parent))
                        }
                    }
                }
            }
        }
    }
}