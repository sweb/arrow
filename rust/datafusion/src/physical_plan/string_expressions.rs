// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

// Some of these functions reference the Postgres documentation
// or implementation to ensure compatibility and are subject to
// the Postgres license.

//! String expressions

use std::any::type_name;
use std::cmp::Ordering;
use std::str::from_utf8;
use std::sync::Arc;

use crate::{
    error::{DataFusionError, Result},
    scalar::ScalarValue,
};
use arrow::{
    array::{
        Array, ArrayRef, GenericStringArray, Int32Array, Int64Array, PrimitiveArray,
        StringArray, StringOffsetSizeTrait,
    },
    compute,
    datatypes::{ArrowNativeType, ArrowPrimitiveType, DataType},
};
use unicode_segmentation::UnicodeSegmentation;

use super::ColumnarValue;

macro_rules! downcast_string_arg {
    ($ARG:expr, $NAME:expr, $T:ident) => {{
        $ARG.as_any()
            .downcast_ref::<GenericStringArray<T>>()
            .ok_or_else(|| {
                DataFusionError::Internal(format!(
                    "could not cast {} to {}",
                    $NAME,
                    type_name::<GenericStringArray<T>>()
                ))
            })?
    }};
}

macro_rules! downcast_primitive_array_arg {
    ($ARG:expr, $NAME:expr, $T:ident) => {{
        $ARG.as_any()
            .downcast_ref::<PrimitiveArray<T>>()
            .ok_or_else(|| {
                DataFusionError::Internal(format!(
                    "could not cast {} to {}",
                    $NAME,
                    type_name::<PrimitiveArray<T>>()
                ))
            })?
    }};
}

macro_rules! downcast_arg {
    ($ARG:expr, $NAME:expr, $ARRAY_TYPE:ident) => {{
        $ARG.as_any().downcast_ref::<$ARRAY_TYPE>().ok_or_else(|| {
            DataFusionError::Internal(format!(
                "could not cast {} to {}",
                $NAME,
                type_name::<$ARRAY_TYPE>()
            ))
        })?
    }};
}

macro_rules! downcast_vec {
    ($ARGS:expr, $ARRAY_TYPE:ident) => {{
        $ARGS
            .iter()
            .map(|e| match e.as_any().downcast_ref::<$ARRAY_TYPE>() {
                Some(array) => Ok(array),
                _ => Err(DataFusionError::Internal("failed to downcast".to_string())),
            })
    }};
}

/// applies a unary expression to `args[0]` that is expected to be downcastable to
/// a `GenericStringArray` and returns a `GenericStringArray` (which may have a different offset)
/// # Errors
/// This function errors when:
/// * the number of arguments is not 1
/// * the first argument is not castable to a `GenericStringArray`
pub(crate) fn unary_string_function<'a, T, O, F, R>(
    args: &[&'a dyn Array],
    op: F,
    name: &str,
) -> Result<GenericStringArray<O>>
where
    R: AsRef<str>,
    O: StringOffsetSizeTrait,
    T: StringOffsetSizeTrait,
    F: Fn(&'a str) -> R,
{
    if args.len() != 1 {
        return Err(DataFusionError::Internal(format!(
            "{:?} args were supplied but {} takes exactly one argument",
            args.len(),
            name,
        )));
    }

    let string_array = downcast_string_arg!(args[0], "string", T);

    // first map is the iterator, second is for the `Option<_>`
    Ok(string_array
        .iter()
        .map(|string| string.map(|string| op(string)))
        .collect())
}

fn handle<'a, F, R>(args: &'a [ColumnarValue], op: F, name: &str) -> Result<ColumnarValue>
where
    R: AsRef<str>,
    F: Fn(&'a str) -> R,
{
    match &args[0] {
        ColumnarValue::Array(a) => match a.data_type() {
            DataType::Utf8 => {
                Ok(ColumnarValue::Array(Arc::new(unary_string_function::<
                    i32,
                    i32,
                    _,
                    _,
                >(
                    &[a.as_ref()], op, name
                )?)))
            }
            DataType::LargeUtf8 => {
                Ok(ColumnarValue::Array(Arc::new(unary_string_function::<
                    i64,
                    i64,
                    _,
                    _,
                >(
                    &[a.as_ref()], op, name
                )?)))
            }
            other => Err(DataFusionError::Internal(format!(
                "Unsupported data type {:?} for function {}",
                other, name,
            ))),
        },
        ColumnarValue::Scalar(scalar) => match scalar {
            ScalarValue::Utf8(a) => {
                let result = a.as_ref().map(|x| (op)(x).as_ref().to_string());
                Ok(ColumnarValue::Scalar(ScalarValue::Utf8(result)))
            }
            ScalarValue::LargeUtf8(a) => {
                let result = a.as_ref().map(|x| (op)(x).as_ref().to_string());
                Ok(ColumnarValue::Scalar(ScalarValue::LargeUtf8(result)))
            }
            other => Err(DataFusionError::Internal(format!(
                "Unsupported data type {:?} for function {}",
                other, name,
            ))),
        },
    }
}

/// Returns the numeric code of the first character of the argument.
/// ascii('x') = 120
pub fn ascii<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);

    let result = string_array
        .iter()
        .map(|string| {
            string.map(|string: &str| {
                let mut chars = string.chars();
                chars.next().map_or(0, |v| v as i32)
            })
        })
        .collect::<Int32Array>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Removes the longest string containing only characters in characters (a space by default) from the start and end of string.
/// btrim('xyxtrimyyx', 'xyz') = 'trim'
pub fn btrim<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        1 => {
            let string_array = downcast_string_arg!(args[0], "string", T);

            let result = string_array
                .iter()
                .map(|string| {
                    string.map(|string: &str| {
                        string.trim_start_matches(' ').trim_end_matches(' ')
                    })
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let characters_array = downcast_string_arg!(args[1], "characters", T);

            let result = string_array
                .iter()
                .zip(characters_array.iter())
                .map(|(string, characters)| match (string, characters) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(characters)) => {
                        let chars: Vec<char> = characters.chars().collect();
                        Some(
                            string
                                .trim_start_matches(&chars[..])
                                .trim_end_matches(&chars[..]),
                        )
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "btrim was called with {} arguments. It requires at least 1 and at most 2.",
            other
        ))),
    }
}

/// Returns number of characters in the string.
/// character_length('josé') = 4
pub fn character_length<T: ArrowPrimitiveType>(args: &[ArrayRef]) -> Result<ArrayRef>
where
    T::Native: StringOffsetSizeTrait,
{
    let string_array: &GenericStringArray<T::Native> = args[0]
        .as_any()
        .downcast_ref::<GenericStringArray<T::Native>>()
        .ok_or_else(|| {
            DataFusionError::Internal("could not cast string to StringArray".to_string())
        })?;

    let result = string_array
        .iter()
        .map(|string| {
            string.map(|string: &str| {
                T::Native::from_usize(string.graphemes(true).count()).unwrap()
            })
        })
        .collect::<PrimitiveArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Returns the character with the given code. chr(0) is disallowed because text data types cannot store that character.
/// chr(65) = 'A'
pub fn chr(args: &[ArrayRef]) -> Result<ArrayRef> {
    let integer_array = downcast_arg!(args[0], "integer", Int64Array);

    // first map is the iterator, second is for the `Option<_>`
    let result = integer_array
        .iter()
        .map(|integer: Option<i64>| {
            integer
                .map(|integer| {
                    if integer == 0 {
                        Err(DataFusionError::Execution(
                            "null character not permitted.".to_string(),
                        ))
                    } else {
                        match core::char::from_u32(integer as u32) {
                            Some(integer) => Ok(integer.to_string()),
                            None => Err(DataFusionError::Execution(
                                "requested character too large for encoding.".to_string(),
                            )),
                        }
                    }
                })
                .transpose()
        })
        .collect::<Result<StringArray>>()?;

    Ok(Arc::new(result) as ArrayRef)
}

/// Concatenates the text representations of all the arguments. NULL arguments are ignored.
/// concat('abcde', 2, NULL, 22) = 'abcde222'
pub fn concat(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    // do not accept 0 arguments.
    if args.is_empty() {
        return Err(DataFusionError::Internal(format!(
            "concat was called with {} arguments. It requires at least 1.",
            args.len()
        )));
    }

    // first, decide whether to return a scalar or a vector.
    let mut return_array = args.iter().filter_map(|x| match x {
        ColumnarValue::Array(array) => Some(array.len()),
        _ => None,
    });
    if let Some(size) = return_array.next() {
        let result = (0..size)
            .map(|index| {
                let mut owned_string: String = "".to_owned();
                for arg in args {
                    match arg {
                        ColumnarValue::Scalar(ScalarValue::Utf8(maybe_value)) => {
                            if let Some(value) = maybe_value {
                                owned_string.push_str(value);
                            }
                        }
                        ColumnarValue::Array(v) => {
                            if v.is_valid(index) {
                                let v = v.as_any().downcast_ref::<StringArray>().unwrap();
                                owned_string.push_str(&v.value(index));
                            }
                        }
                        _ => unreachable!(),
                    }
                }
                Some(owned_string)
            })
            .collect::<StringArray>();

        Ok(ColumnarValue::Array(Arc::new(result)))
    } else {
        // short avenue with only scalars
        let initial = Some("".to_string());
        let result = args.iter().fold(initial, |mut acc, rhs| {
            if let Some(ref mut inner) = acc {
                match rhs {
                    ColumnarValue::Scalar(ScalarValue::Utf8(Some(v))) => {
                        inner.push_str(v);
                    }
                    ColumnarValue::Scalar(ScalarValue::Utf8(None)) => {}
                    _ => unreachable!(""),
                };
            };
            acc
        });
        Ok(ColumnarValue::Scalar(ScalarValue::Utf8(result)))
    }
}

/// Concatenates all but the first argument, with separators. The first argument is used as the separator string, and should not be NULL. Other NULL arguments are ignored.
/// concat_ws(',', 'abcde', 2, NULL, 22) = 'abcde,2,22'
pub fn concat_ws(args: &[ArrayRef]) -> Result<ArrayRef> {
    // downcast all arguments to strings
    let args = downcast_vec!(args, StringArray).collect::<Result<Vec<&StringArray>>>()?;

    // do not accept 0 or 1 arguments.
    if args.len() < 2 {
        return Err(DataFusionError::Internal(format!(
            "concat_ws was called with {} arguments. It requires at least 2.",
            args.len()
        )));
    }

    // first map is the iterator, second is for the `Option<_>`
    let result = args[0]
        .iter()
        .enumerate()
        .map(|(index, x)| {
            x.map(|sep: &str| {
                let mut owned_string: String = "".to_owned();
                for arg_index in 1..args.len() {
                    let arg = &args[arg_index];
                    if !arg.is_null(index) {
                        owned_string.push_str(&arg.value(index));
                        // if not last push separator
                        if arg_index != args.len() - 1 {
                            owned_string.push_str(&sep);
                        }
                    }
                }
                owned_string
            })
        })
        .collect::<StringArray>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Converts the first letter of each word to upper case and the rest to lower case. Words are sequences of alphanumeric characters separated by non-alphanumeric characters.
/// initcap('hi THOMAS') = 'Hi Thomas'
pub fn initcap<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);

    // first map is the iterator, second is for the `Option<_>`
    let result = string_array
        .iter()
        .map(|string| {
            string.map(|string: &str| {
                let mut char_vector = Vec::<char>::new();
                let mut previous_character_letter_or_number = false;
                for c in string.chars() {
                    if previous_character_letter_or_number {
                        char_vector.push(c.to_ascii_lowercase());
                    } else {
                        char_vector.push(c.to_ascii_uppercase());
                    }
                    previous_character_letter_or_number = ('A'..='Z').contains(&c)
                        || ('a'..='z').contains(&c)
                        || ('0'..='9').contains(&c);
                }
                char_vector.iter().collect::<String>()
            })
        })
        .collect::<GenericStringArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Returns first n characters in the string, or when n is negative, returns all but last |n| characters.
/// left('abcde', 2) = 'ab'
pub fn left<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);
    let n_array = downcast_arg!(args[1], "n", Int64Array);

    let result = string_array
        .iter()
        .zip(n_array.iter())
        .map(|(string, n)| match (string, n) {
            (None, _) => None,
            (_, None) => None,
            (Some(string), Some(n)) => match n.cmp(&0) {
                Ordering::Equal => Some(""),
                Ordering::Greater => Some(
                    string
                        .grapheme_indices(true)
                        .nth(n as usize)
                        .map_or(string, |(i, _)| {
                            &from_utf8(&string.as_bytes()[..i]).unwrap()
                        }),
                ),
                Ordering::Less => Some(
                    string
                        .grapheme_indices(true)
                        .rev()
                        .nth(n.abs() as usize - 1)
                        .map_or("", |(i, _)| {
                            &from_utf8(&string.as_bytes()[..i]).unwrap()
                        }),
                ),
            },
        })
        .collect::<GenericStringArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Converts the string to all lower case.
/// lower('TOM') = 'tom'
pub fn lower(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle(args, |string| string.to_ascii_lowercase(), "lower")
}

/// Extends the string to length length by prepending the characters fill (a space by default). If the string is already longer than length then it is truncated (on the right).
/// lpad('hi', 5, 'xy') = 'xyxhi'
pub fn lpad<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let length_array = downcast_arg!(args[1], "length", Int64Array);

            let result = string_array
                .iter()
                .zip(length_array.iter())
                .map(|(string, length)| match (string, length) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(length)) => {
                        let length = length as usize;
                        if length == 0 {
                            Some("".to_string())
                        } else {
                            let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                            if length < graphemes.len() {
                                Some(graphemes[..length].concat())
                            } else {
                                let mut s = string.to_string();
                                s.insert_str(
                                    0,
                                    " ".repeat(length - graphemes.len()).as_str(),
                                );
                                Some(s)
                            }
                        }
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        3 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let length_array = downcast_arg!(args[1], "length", Int64Array);
            let fill_array = downcast_string_arg!(args[2], "fill", T);

            let result = string_array
                .iter()
                .zip(length_array.iter())
                .zip(fill_array.iter())
                .map(|((string, length), fill)| match (string, length, fill) {
                    (None, _, _) => None,
                    (_, None, _) => None,
                    (_, _, None) => None,
                    (Some(string), Some(length), Some(fill)) => {
                        let length = length as usize;

                        if length == 0 {
                            Some("".to_string())
                        } else {
                            let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                            let fill_chars = fill.chars().collect::<Vec<char>>();

                            if length < graphemes.len() {
                                Some(graphemes[..length].concat())
                            } else if fill_chars.is_empty() {
                                Some(string.to_string())
                            } else {
                                let mut s = string.to_string();
                                let mut char_vector =
                                    Vec::<char>::with_capacity(length - graphemes.len());
                                for l in 0..length - graphemes.len() {
                                    char_vector.push(
                                        *fill_chars.get(l % fill_chars.len()).unwrap(),
                                    );
                                }
                                s.insert_str(
                                    0,
                                    char_vector.iter().collect::<String>().as_str(),
                                );
                                Some(s)
                            }
                        }
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "lpad was called with {} arguments. It requires at least 2 and at most 3.",
            other
        ))),
    }
}

/// Removes the longest string containing only characters in characters (a space by default) from the start of string.
/// ltrim('zzzytest', 'xyz') = 'test'
pub fn ltrim<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        1 => {
            let string_array = downcast_string_arg!(args[0], "string", T);

            let result = string_array
                .iter()
                .map(|string| string.map(|string: &str| string.trim_start_matches(' ')))
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let characters_array = downcast_string_arg!(args[1], "characters", T);

            let result = string_array
                .iter()
                .zip(characters_array.iter())
                .map(|(string, characters)| match (string, characters) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(characters)) => {
                        let chars: Vec<char> = characters.chars().collect();
                        Some(string.trim_start_matches(&chars[..]))
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "ltrim was called with {} arguments. It requires at least 1 and at most 2.",
            other
        ))),
    }
}

/// extract a specific group from a string column, using a regular expression
pub fn regexp_extract(args: &[ArrayRef]) -> Result<ArrayRef> {
    let pattern_expr = args[1].as_any().downcast_ref::<StringArray>().unwrap();
    let pattern = pattern_expr.value(0);
    let idx_expr = args[2].as_any().downcast_ref::<Int64Array>().unwrap();
    let idx = idx_expr.value(0) as usize;
    compute::regexp_extract(args[0].as_ref(), pattern, idx)
        .map_err(DataFusionError::ArrowError)
}

/// extract a specific group from a string column, using a regular expression
pub fn regexp_match(args: &[ArrayRef]) -> Result<ArrayRef> {
    let pattern_expr = args[1].as_any().downcast_ref::<StringArray>().unwrap();
    let pattern = pattern_expr.value(0);
    compute::regexp_match(args[0].as_ref(), pattern).map_err(DataFusionError::ArrowError)
}

/// Repeats string the specified number of times.
/// repeat('Pg', 4) = 'PgPgPgPg'
pub fn repeat<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);
    let number_array = downcast_arg!(args[1], "number", Int64Array);

    let result = string_array
        .iter()
        .zip(number_array.iter())
        .map(|(string, number)| match (string, number) {
            (None, _) => None,
            (_, None) => None,
            (Some(string), Some(number)) => Some(string.repeat(number as usize)),
        })
        .collect::<GenericStringArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Reverses the order of the characters in the string.
/// reverse('abcde') = 'edcba'
pub fn reverse<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);

    let result = string_array
        .iter()
        .map(|string| {
            string.map(|string: &str| string.graphemes(true).rev().collect::<String>())
        })
        .collect::<GenericStringArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Returns last n characters in the string, or when n is negative, returns all but first |n| characters.
/// right('abcde', 2) = 'de'
pub fn right<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    let string_array = downcast_string_arg!(args[0], "string", T);
    let n_array = downcast_arg!(args[1], "n", Int64Array);

    let result = string_array
        .iter()
        .zip(n_array.iter())
        .map(|(string, n)| match (string, n) {
            (None, _) => None,
            (_, None) => None,
            (Some(string), Some(n)) => match n.cmp(&0) {
                Ordering::Equal => Some(""),
                Ordering::Greater => Some(
                    string
                        .grapheme_indices(true)
                        .rev()
                        .nth(n as usize - 1)
                        .map_or(string, |(i, _)| {
                            &from_utf8(&string.as_bytes()[i..]).unwrap()
                        }),
                ),
                Ordering::Less => Some(
                    string
                        .grapheme_indices(true)
                        .nth(n.abs() as usize)
                        .map_or("", |(i, _)| {
                            &from_utf8(&string.as_bytes()[i..]).unwrap()
                        }),
                ),
            },
        })
        .collect::<GenericStringArray<T>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Extends the string to length length by appending the characters fill (a space by default). If the string is already longer than length then it is truncated.
/// rpad('hi', 5, 'xy') = 'hixyx'
pub fn rpad<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let length_array = downcast_arg!(args[1], "length", Int64Array);

            let result = string_array
                .iter()
                .zip(length_array.iter())
                .map(|(string, length)| match (string, length) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(length)) => {
                        let length = length as usize;
                        if length == 0 {
                            Some("".to_string())
                        } else {
                            let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                            if length < graphemes.len() {
                                Some(graphemes[..length].concat())
                            } else {
                                let mut s = string.to_string();
                                s.push_str(" ".repeat(length - graphemes.len()).as_str());
                                Some(s)
                            }
                        }
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        3 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let length_array = downcast_arg!(args[1], "length", Int64Array);
            let fill_array = downcast_string_arg!(args[2], "fill", T);

            let result = string_array
                .iter()
                .zip(length_array.iter())
                .zip(fill_array.iter())
                .map(|((string, length), fill)| match (string, length, fill) {
                    (None, _, _) => None,
                    (_, None, _) => None,
                    (_, _, None) => None,
                    (Some(string), Some(length), Some(fill)) => {
                        let length = length as usize;
                        let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                        let fill_chars = fill.chars().collect::<Vec<char>>();

                        if length < graphemes.len() {
                            Some(graphemes[..length].concat())
                        } else if fill_chars.is_empty() {
                            Some(string.to_string())
                        } else {
                            let mut s = string.to_string();
                            let mut char_vector =
                                Vec::<char>::with_capacity(length - graphemes.len());
                            for l in 0..length - graphemes.len() {
                                char_vector
                                    .push(*fill_chars.get(l % fill_chars.len()).unwrap());
                            }
                            s.push_str(char_vector.iter().collect::<String>().as_str());
                            Some(s)
                        }
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "rpad was called with {} arguments. It requires at least 2 and at most 3.",
            other
        ))),
    }
}

/// Removes the longest string containing only characters in characters (a space by default) from the end of string.
/// rtrim('testxxzx', 'xyz') = 'test'
pub fn rtrim<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        1 => {
            let string_array = downcast_string_arg!(args[0], "string", T);

            let result = string_array
                .iter()
                .map(|string| string.map(|string: &str| string.trim_end_matches(' ')))
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let characters_array = downcast_string_arg!(args[1], "characters", T);

            let result = string_array
                .iter()
                .zip(characters_array.iter())
                .map(|(string, characters)| match (string, characters) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(characters)) => {
                        let chars: Vec<char> = characters.chars().collect();
                        Some(string.trim_end_matches(&chars[..]))
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "rtrim was called with {} arguments. It requires at least 1 and at most 2.",
            other
        ))),
    }
}

/// Extracts the substring of string starting at the start'th character, and extending for count characters if that is specified. (Same as substring(string from start for count).)
/// substr('alphabet', 3) = 'phabet'
/// substr('alphabet', 3, 2) = 'ph'
pub fn substr<T: StringOffsetSizeTrait>(args: &[ArrayRef]) -> Result<ArrayRef> {
    match args.len() {
        2 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let start_array = downcast_arg!(args[1], "start", Int64Array);

            let result = string_array
                .iter()
                .zip(start_array.iter())
                .map(|(string, start)| match (string, start) {
                    (None, _) => None,
                    (_, None) => None,
                    (Some(string), Some(start)) => {
                        if start <= 0 {
                            Some(string.to_string())
                        } else {
                            let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                            let start_pos = start as usize - 1;
                            if graphemes.len() < start_pos {
                                Some("".to_string())
                            } else {
                                Some(graphemes[start_pos..].concat())
                            }
                        }
                    }
                })
                .collect::<GenericStringArray<T>>();

            Ok(Arc::new(result) as ArrayRef)
        }
        3 => {
            let string_array = downcast_string_arg!(args[0], "string", T);
            let start_array = downcast_arg!(args[1], "start", Int64Array);
            let count_array = downcast_arg!(args[2], "count", Int64Array);

            let result = string_array
                .iter()
                .zip(start_array.iter())
                .zip(count_array.iter())
                .map(|((string, start), count)| match (string, start, count) {
                    (None, _, _) => Ok(None),
                    (_, None, _) => Ok(None),
                    (_, _, None) => Ok(None),
                    (Some(string), Some(start), Some(count)) => {
                        if count < 0 {
                            Err(DataFusionError::Execution(
                                "negative substring length not allowed".to_string(),
                            ))
                        } else if start <= 0 {
                            Ok(Some(string.to_string()))
                        } else {
                            let graphemes = string.graphemes(true).collect::<Vec<&str>>();
                            let start_pos = start as usize - 1;
                            let count_usize = count as usize;
                            if graphemes.len() < start_pos {
                                Ok(Some("".to_string()))
                            } else if graphemes.len() < start_pos + count_usize {
                                Ok(Some(graphemes[start_pos..].concat()))
                            } else {
                                Ok(Some(
                                    graphemes[start_pos..start_pos + count_usize]
                                        .concat(),
                                ))
                            }
                        }
                    }
                })
                .collect::<Result<GenericStringArray<T>>>()?;

            Ok(Arc::new(result) as ArrayRef)
        }
        other => Err(DataFusionError::Internal(format!(
            "substr was called with {} arguments. It requires 2 or 3.",
            other
        ))),
    }
}

/// Converts the number to its equivalent hexadecimal representation.
/// to_hex(2147483647) = '7fffffff'
pub fn to_hex<T: ArrowPrimitiveType>(args: &[ArrayRef]) -> Result<ArrayRef>
where
    T::Native: StringOffsetSizeTrait,
{
    let integer_array = downcast_primitive_array_arg!(args[0], "integer", T);

    let result = integer_array
        .iter()
        .map(|integer| {
            integer.map(|integer| format!("{:x}", integer.to_usize().unwrap()))
        })
        .collect::<GenericStringArray<i32>>();

    Ok(Arc::new(result) as ArrayRef)
}

/// Converts the string to all upper case.
/// upper('tom') = 'TOM'
pub fn upper(args: &[ColumnarValue]) -> Result<ColumnarValue> {
    handle(args, |string| string.to_ascii_uppercase(), "upper")
}
