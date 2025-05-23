// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Methods for reading / writing oximeter fields to the database.

// Copyright 2024 Oxide Computer Company

use super::columns;
use super::from_block::FromBlock;
use crate::Metric;
use crate::Target;
use crate::native::Error;
use crate::native::block::Block;
use crate::native::block::Column;
use crate::native::block::DataType;
use crate::native::block::ValueArray;
use crate::query::field_table_name;
use indexmap::IndexMap;
use oximeter::Field;
use oximeter::FieldSchema;
use oximeter::FieldSource;
use oximeter::FieldType;
use oximeter::FieldValue;
use oximeter::Sample;
use oximeter::TimeseriesSchema;
use oximeter::schema::TimeseriesKey;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::net::IpAddr;

/// A row selected from a "field select query", used in the older query
/// interface.
///
/// This is used in `select_matching_timeseries_info()` to pull out the fields
/// that match a query.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSelectRow {
    pub timeseries_key: TimeseriesKey,
    pub target: Target,
    pub metric: Metric,
}

impl FromBlock for FieldSelectRow {
    type Context = TimeseriesSchema;

    fn from_block(
        block: &Block,
        schema: &Self::Context,
    ) -> Result<Vec<Self>, Error> {
        let n_rows = block.n_rows();
        if n_rows == 0 {
            return Ok(vec![]);
        }
        let timeseries_keys = block
            .column_values(columns::TIMESERIES_KEY)?
            .as_u64()
            .map_err(|actual| {
                Error::unexpected_column_type(
                    block,
                    columns::TIMESERIES_KEY,
                    actual.to_string(),
                )
            })?;
        let field_rows =
            extract_field_rows_from_block(block, &schema.field_schema)?;
        let mut out = Vec::with_capacity(n_rows);
        let (target_name, metric_name) = schema.component_names();
        for (timeseries_key, field_rows) in
            timeseries_keys.iter().copied().zip(field_rows)
        {
            out.push(FieldSelectRow {
                timeseries_key,
                target: Target {
                    name: target_name.to_string(),
                    fields: field_rows.target,
                },
                metric: Metric {
                    name: metric_name.to_string(),
                    fields: field_rows.metric,
                    datum_type: schema.datum_type,
                },
            });
        }
        Ok(out)
    }
}

struct FieldRow {
    target: Vec<Field>,
    metric: Vec<Field>,
}

/// Extract the fields from every row in `block`, using the provided schema.
fn extract_field_rows_from_block(
    block: &Block,
    field_schema: &BTreeSet<FieldSchema>,
) -> Result<Vec<FieldRow>, Error> {
    let mut out = Vec::with_capacity(block.n_rows());
    for row in 0..block.n_rows() {
        let mut target = Vec::with_capacity(1);
        let mut metric = Vec::new();
        for field in field_schema.iter() {
            let col = block.column_values(&field.name)?;
            let field_value = match field.field_type {
                FieldType::String => {
                    let ValueArray::String(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "String",
                        ));
                    };
                    FieldValue::from(x[row].clone())
                }
                FieldType::I8 => {
                    let ValueArray::Int8(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Int8",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::U8 => {
                    let ValueArray::UInt8(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "UInt8",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::I16 => {
                    let ValueArray::Int16(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Int16",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::U16 => {
                    let ValueArray::UInt16(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "UInt16",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::I32 => {
                    let ValueArray::Int32(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Int32",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::U32 => {
                    let ValueArray::UInt32(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "UInt32",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::I64 => {
                    let ValueArray::Int64(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Int64",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::U64 => {
                    let ValueArray::UInt64(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "String",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::IpAddr => {
                    let ValueArray::Ipv6(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "IpAddr",
                        ));
                    };
                    let v6 = x[row];
                    let addr = match v6.to_ipv4_mapped() {
                        Some(v4) => IpAddr::V4(v4),
                        None => IpAddr::V6(v6),
                    };
                    FieldValue::from(addr)
                }
                FieldType::Uuid => {
                    let ValueArray::Uuid(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Uuid",
                        ));
                    };
                    FieldValue::from(x[row])
                }
                FieldType::Bool => {
                    let ValueArray::Bool(x) = col else {
                        return Err(Error::unexpected_column_type(
                            block,
                            &field.name,
                            "Bool",
                        ));
                    };
                    FieldValue::from(x[row])
                }
            };
            let this_field =
                Field { name: field.name.clone(), value: field_value };
            match field.source {
                FieldSource::Target => target.push(this_field),
                FieldSource::Metric => metric.push(this_field),
            }
        }
        out.push(FieldRow { target, metric });
    }
    Ok(out)
}

/// Extract `Block`s for all fields in a `Sample`.
///
/// This returns a data block for each field table, which can be inserted into
/// the database.
pub(crate) fn extract_fields_as_block(
    sample: &Sample,
) -> BTreeMap<String, Block> {
    let mut out = BTreeMap::new();
    let timeseries_key = crate::timeseries_key(sample);
    for field in sample.fields() {
        let field_type = field.value.field_type();
        let table_name = field_table_name(field_type);
        let entry = out.entry(table_name).or_insert_with(|| Block {
            name: String::new(),
            info: Default::default(),
            columns: empty_columns_for_field(field_type),
        });

        // Push the timeseries name, key, and field name.
        let Ok(ValueArray::String(timeseries_names)) =
            entry.column_values_mut(columns::TIMESERIES_NAME)
        else {
            unreachable!();
        };
        timeseries_names.push(sample.timeseries_name.to_string());
        let Ok(ValueArray::UInt64(keys)) =
            entry.column_values_mut(columns::TIMESERIES_KEY)
        else {
            unreachable!();
        };
        keys.push(timeseries_key);
        let Ok(ValueArray::String(field_names)) =
            entry.column_values_mut(columns::FIELD_NAME)
        else {
            unreachable!();
        };
        field_names.push(field.name.clone());

        // Push the field value, which depends on the type.
        let values = entry.column_values_mut(columns::FIELD_VALUE).unwrap();
        match (field.value, values) {
            (FieldValue::String(x), ValueArray::String(values)) => {
                values.push(x.to_string())
            }
            (FieldValue::I8(x), ValueArray::Int8(values)) => values.push(x),
            (FieldValue::U8(x), ValueArray::UInt8(values)) => values.push(x),
            (FieldValue::I16(x), ValueArray::Int16(values)) => values.push(x),
            (FieldValue::U16(x), ValueArray::UInt16(values)) => values.push(x),
            (FieldValue::I32(x), ValueArray::Int32(values)) => values.push(x),
            (FieldValue::U32(x), ValueArray::UInt32(values)) => values.push(x),
            (FieldValue::I64(x), ValueArray::Int64(values)) => values.push(x),
            (FieldValue::U64(x), ValueArray::UInt64(values)) => values.push(x),
            (FieldValue::IpAddr(x), ValueArray::Ipv6(values)) => {
                let addr = match x {
                    std::net::IpAddr::V4(v4) => v4.to_ipv6_mapped(),
                    std::net::IpAddr::V6(v6) => v6,
                };
                values.push(addr);
            }
            (FieldValue::Uuid(x), ValueArray::Uuid(values)) => values.push(x),
            (FieldValue::Bool(x), ValueArray::Bool(values)) => values.push(x),
            (_, _) => unreachable!(),
        }
    }
    out
}

/// Construct an empty set of columns for a field table of the given type.
fn empty_columns_for_field(field_type: FieldType) -> IndexMap<String, Column> {
    IndexMap::from([
        (
            String::from(columns::TIMESERIES_NAME),
            Column::from(ValueArray::empty(&DataType::String)),
        ),
        (
            String::from(columns::TIMESERIES_KEY),
            Column::from(ValueArray::empty(&DataType::UInt64)),
        ),
        (
            String::from(columns::FIELD_NAME),
            Column::from(ValueArray::empty(&DataType::String)),
        ),
        (
            String::from(columns::FIELD_VALUE),
            Column::from(ValueArray::empty(&DataType::from(field_type))),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::columns;
    use super::extract_fields_as_block;
    use crate::native::block::ValueArray;
    use oximeter::Sample;

    #[derive(oximeter::Target)]
    struct SomeTarget {
        name: String,
        id: uuid::Uuid,
        other_name: String,
    }

    #[derive(oximeter::Metric, Default)]
    struct SomeMetric {
        yet_another_name: String,
        datum: u64,
    }

    #[test]
    fn test_extract_fields_as_block() {
        let t = SomeTarget {
            name: String::from("bill"),
            id: uuid::Uuid::new_v4(),
            other_name: String::from("ted"),
        };
        let m = SomeMetric { yet_another_name: String::from("tim"), datum: 0 };
        let sample = Sample::new(&t, &m).unwrap();
        let blocks = extract_fields_as_block(&sample);
        assert_eq!(blocks.len(), 2, "Should extract blocks for 2 field tables");
        let block = blocks
            .get("fields_string")
            .expect("Should have created a block for the fields_string table");
        assert_eq!(
            block.n_columns(),
            4,
            "Blocks for the field tables should list each column in the table schema",
        );
        assert_eq!(
            block.n_rows(),
            3,
            "Should have extracted 3 rows for the string field table"
        );

        let strings = block
            .column_values(columns::FIELD_VALUE)
            .expect("Should have a column named `field_value`");
        let ValueArray::String(strings) = strings else {
            panic!("Expected an array of strings, found: {strings:?}");
        };
        let mut strings = strings.clone();
        strings.sort();
        assert_eq!(
            strings,
            &["bill", "ted", "tim"],
            "Incorrect field values for the string fields"
        );

        let block = blocks
            .get("fields_uuid")
            .expect("Should have created a block for the fields_uuid table");
        assert_eq!(
            block.n_columns(),
            4,
            "Blocks for the field tables should list each column in the table schema",
        );
        assert_eq!(
            block.n_rows(),
            1,
            "Should have extracted 1 row for the UUID field table"
        );
        let ids = block
            .column_values(columns::FIELD_VALUE)
            .expect("Should have a column named `field_value`");
        let ValueArray::Uuid(ids) = ids else {
            panic!("Expected an array of strings, found: {ids:?}");
        };
        assert_eq!(ids, &[t.id], "Incorrect field values for the UUID fields");
    }
}
