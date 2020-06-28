//! Deserializer that reads a file and creates a debug representation of it.
//! It's intended to be used to snapshot test the binary serializer without
//! suffering from same-inverse-bug problems.

#![allow(missing_docs)]

use std::{collections::HashMap, convert::TryInto, io::Read};

use byteorder::{LittleEndian, ReadBytesExt};
use rbx_dom_weak::types::{Color3, UDim, UDim2};
use serde::{Deserialize, Serialize};

use crate::{chunk::Chunk, core::RbxReadExt, deserializer::FileHeader, types::Type};

#[derive(Debug, Serialize, Deserialize)]
pub struct DecodedModel {
    pub num_types: u32,
    pub num_instances: u32,
    pub chunks: Vec<DecodedChunk>,
}

impl DecodedModel {
    pub fn from_reader<R: Read>(mut reader: R) -> Self {
        let header = FileHeader::decode(&mut reader).expect("invalid file header");
        let mut chunks = Vec::new();

        // The number of instance with a given type ID. Used to correctly decode
        // lists of properties from the PROP chunk.
        let mut count_by_type_id = HashMap::new();

        loop {
            let chunk = Chunk::decode(&mut reader).expect("invalid chunk");

            match &chunk.name {
                b"META" => chunks.push(decode_meta_chunk(chunk.data.as_slice())),
                b"INST" => chunks.push(decode_inst_chunk(
                    chunk.data.as_slice(),
                    &mut count_by_type_id,
                )),
                b"PROP" => chunks.push(decode_prop_chunk(
                    chunk.data.as_slice(),
                    &mut count_by_type_id,
                )),
                b"PRNT" => chunks.push(decode_prnt_chunk(chunk.data.as_slice())),
                b"END\0" => {
                    chunks.push(DecodedChunk::End);
                    break;
                }
                _ => {
                    chunks.push(DecodedChunk::Unknown {
                        name: String::from_utf8_lossy(&chunk.name[..]).to_string(),
                        contents: chunk.data,
                    });
                }
            }
        }

        DecodedModel {
            num_types: header.num_types,
            num_instances: header.num_instances,
            chunks,
        }
    }
}

fn decode_meta_chunk<R: Read>(mut reader: R) -> DecodedChunk {
    let num_entries = reader.read_u32::<LittleEndian>().unwrap();
    let mut entries = Vec::with_capacity(num_entries as usize);

    for _ in 0..num_entries {
        let key = reader.read_string().unwrap();
        let value = reader.read_string().unwrap();
        entries.push((key, value));
    }

    let mut remaining = Vec::new();
    reader.read_to_end(&mut remaining).unwrap();

    DecodedChunk::Meta { entries, remaining }
}

fn decode_inst_chunk<R: Read>(
    mut reader: R,
    count_by_type_id: &mut HashMap<u32, usize>,
) -> DecodedChunk {
    let type_id = reader.read_u32::<LittleEndian>().unwrap();
    let type_name = reader.read_string().unwrap();
    let object_format = reader.read_u8().unwrap();
    let num_instances = reader.read_u32::<LittleEndian>().unwrap();

    count_by_type_id.insert(type_id, num_instances as usize);

    let mut referents = vec![0; num_instances as usize];
    reader.read_referent_array(&mut referents).unwrap();

    let mut remaining = Vec::new();
    reader.read_to_end(&mut remaining).unwrap();

    DecodedChunk::Inst {
        type_id,
        type_name,
        object_format,
        referents,
        remaining,
    }
}

fn decode_prop_chunk<R: Read>(
    mut reader: R,
    count_by_type_id: &mut HashMap<u32, usize>,
) -> DecodedChunk {
    let type_id = reader.read_u32::<LittleEndian>().unwrap();
    let prop_name = reader.read_string().unwrap();

    let prop_type_value = reader.read_u8().unwrap();
    let (prop_type, values) = match prop_type_value.try_into() {
        Ok(prop_type) => {
            // If this type ID is unknown, we'll default to assuming that type
            // has no members and thus has no values of this property.
            let values = count_by_type_id
                .get(&type_id)
                .map(|&prop_count| DecodedValues::decode(&mut reader, prop_count, prop_type))
                .unwrap_or(None);

            (DecodedPropType::Known(prop_type), values)
        }
        Err(_) => (DecodedPropType::Unknown(prop_type_value), None),
    };

    let mut remaining = Vec::new();
    reader.read_to_end(&mut remaining).unwrap();

    DecodedChunk::Prop {
        type_id,
        prop_name,
        prop_type,
        values,
        remaining,
    }
}

fn decode_prnt_chunk<R: Read>(mut reader: R) -> DecodedChunk {
    let version = reader.read_u8().unwrap();
    let num_referents = reader.read_u32::<LittleEndian>().unwrap();

    let mut subjects = vec![0; num_referents as usize];
    let mut parents = vec![0; num_referents as usize];

    reader.read_referent_array(&mut subjects).unwrap();
    reader.read_referent_array(&mut parents).unwrap();

    let links = subjects
        .iter()
        .copied()
        .zip(parents.iter().copied())
        .collect();

    let mut remaining = Vec::new();
    reader.read_to_end(&mut remaining).unwrap();

    DecodedChunk::Prnt {
        version,
        links,
        remaining,
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecodedValues {
    String(Vec<RobloxString>),
    Bool(Vec<bool>),
    Int32(Vec<i32>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    UDim(Vec<UDim>),
    UDim2(Vec<UDim2>),
    Color3(Vec<Color3>),
    Vector2(Vec<f32>, Vec<f32>),
    Int64(Vec<i64>),
}

impl DecodedValues {
    fn decode<R: Read>(mut reader: R, prop_count: usize, prop_type: Type) -> Option<Self> {
        match prop_type {
            Type::String => {
                let mut values = Vec::with_capacity(prop_count);

                for _ in 0..prop_count {
                    values.push(reader.read_binary_string().unwrap().into());
                }

                Some(DecodedValues::String(values))
            }
            Type::Bool => {
                let mut values = Vec::with_capacity(prop_count);

                for _ in 0..prop_count {
                    values.push(reader.read_bool().unwrap());
                }

                Some(DecodedValues::Bool(values))
            }
            Type::Int32 => {
                let mut values = vec![0; prop_count];

                reader.read_interleaved_i32_array(&mut values).unwrap();

                Some(DecodedValues::Int32(values))
            }
            Type::Float32 => {
                let mut values = vec![0.0; prop_count];

                reader.read_interleaved_f32_array(&mut values).unwrap();

                Some(DecodedValues::Float32(values))
            }
            Type::Float64 => {
                let mut values = Vec::with_capacity(prop_count);

                for _ in 0..prop_count {
                    values.push(reader.read_f64::<LittleEndian>().unwrap())
                }

                Some(DecodedValues::Float64(values))
            }
            Type::UDim => {
                let mut scale = vec![0.0; prop_count];
                let mut offset = vec![0; prop_count];

                reader.read_interleaved_f32_array(&mut scale).unwrap();
                reader.read_interleaved_i32_array(&mut offset).unwrap();

                let values = scale
                    .into_iter()
                    .zip(offset.into_iter())
                    .map(|(scale, offset)| UDim::new(scale, offset))
                    .collect();

                Some(DecodedValues::UDim(values))
            }
            Type::UDim2 => {
                let mut scale_x = vec![0.0; prop_count];
                let mut scale_y = vec![0.0; prop_count];
                let mut offset_x = vec![0; prop_count];
                let mut offset_y = vec![0; prop_count];

                reader.read_interleaved_f32_array(&mut scale_x).unwrap();
                reader.read_interleaved_f32_array(&mut scale_y).unwrap();
                reader.read_interleaved_i32_array(&mut offset_x).unwrap();
                reader.read_interleaved_i32_array(&mut offset_y).unwrap();

                let x_values = scale_x
                    .into_iter()
                    .zip(offset_x.into_iter())
                    .map(|(scale, offset)| UDim::new(scale, offset));
                let y_values = scale_y
                    .into_iter()
                    .zip(offset_y.into_iter())
                    .map(|(scale, offset)| UDim::new(scale, offset));

                let values = x_values
                    .zip(y_values)
                    .map(|(x, y)| UDim2::new(x, y))
                    .collect();

                Some(DecodedValues::UDim2(values))
            }
            Type::Color3 => {
                let mut r = vec![0.0; prop_count];
                let mut g = vec![0.0; prop_count];
                let mut b = vec![0.0; prop_count];

                reader.read_interleaved_f32_array(&mut r).unwrap();
                reader.read_interleaved_f32_array(&mut g).unwrap();
                reader.read_interleaved_f32_array(&mut b).unwrap();

                let values = r
                    .into_iter()
                    .zip(g.into_iter())
                    .zip(b.into_iter())
                    .map(|((r, g), b)| Color3::new(r, g, b))
                    .collect();

                Some(DecodedValues::Color3(values))
            }
            Type::Vector2 => {
                let mut x = vec![0.0; prop_count];
                let mut y = vec![0.0; prop_count];

                reader.read_interleaved_f32_array(&mut x).unwrap();
                reader.read_interleaved_f32_array(&mut y).unwrap();

                Some(DecodedValues::Vector2(x, y))
            }
            Type::Int64 => {
                let mut values = vec![0; prop_count];

                reader.read_interleaved_i64_array(&mut values).unwrap();

                Some(DecodedValues::Int64(values))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecodedPropType {
    Known(Type),
    Unknown(u8),
}

/// Holds a string with the same semantics as Roblox does. It can be UTF-8, but
/// might not be.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RobloxString {
    String(String),
    BinaryString(#[serde(with = "unknown_buffer")] Vec<u8>),
}

impl From<Vec<u8>> for RobloxString {
    fn from(value: Vec<u8>) -> Self {
        match String::from_utf8(value) {
            Ok(string) => RobloxString::String(string),
            Err(err) => RobloxString::BinaryString(err.into_bytes()),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum DecodedChunk {
    Meta {
        entries: Vec<(String, String)>,

        #[serde(with = "unknown_buffer", skip_serializing_if = "Vec::is_empty")]
        remaining: Vec<u8>,
    },

    Inst {
        type_id: u32,
        type_name: String,
        object_format: u8,
        referents: Vec<i32>,

        #[serde(with = "unknown_buffer", skip_serializing_if = "Vec::is_empty")]
        remaining: Vec<u8>,
    },

    Prop {
        type_id: u32,
        prop_name: String,
        prop_type: DecodedPropType,

        #[serde(skip_serializing_if = "Option::is_none")]
        values: Option<DecodedValues>,

        #[serde(with = "unknown_buffer", skip_serializing_if = "Vec::is_empty")]
        remaining: Vec<u8>,
    },

    Prnt {
        version: u8,
        links: Vec<(i32, i32)>,

        #[serde(with = "unknown_buffer", skip_serializing_if = "Vec::is_empty")]
        remaining: Vec<u8>,
    },

    End,

    Unknown {
        name: String,

        #[serde(with = "unknown_buffer")]
        contents: Vec<u8>,
    },
}

/// Contains data that we haven't decoded for a chunk. Using `unknown_buffer`
/// should generally be a placeholder since it's results are opaque, but stable.
mod unknown_buffer {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(&base64::display::Base64Display::with_config(
            &value,
            base64::STANDARD,
        ))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = <&str>::deserialize(deserializer)?;
        let contents = base64::decode(encoded).map_err(serde::de::Error::custom)?;

        Ok(contents)
    }
}
