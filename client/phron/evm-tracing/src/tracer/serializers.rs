use ethereum_types::{H256, U256};
use serde::{
	ser::{Error, SerializeSeq},
	Serializer,
};

// Serialize a sequence of H256 values
pub fn serialize_seq_h256<S>(data: &Option<Vec<H256>>, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	match data {
		Some(vec) => {
			let mut seq = serializer.serialize_seq(Some(vec.len()))?; // Start serializing a sequence with the given length
			for hash in vec {
				seq.serialize_element(&format!("{:x}", hash))?; // Serialize each H256 element in hexadecimal format
			}
			seq.end() // Finish serializing the sequence
		}
		None => serializer.serialize_seq(None)?.end(), // If data is None, serialize an empty sequence
	}
}

// Serialize a byte array as a hexadecimal string with "0x" prefix
pub fn serialize_bytes_0x<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&format!("0x{}", hex::encode(bytes))) // Serialize byte array as a hexadecimal string with "0x" prefix
}

// Serialize an optional byte array as a hexadecimal string with "0x" prefix
pub fn serialize_option_bytes_0x<S>(
	bytes: &Option<Vec<u8>>,
	serializer: S,
) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	match bytes {
		Some(bytes) => serializer.serialize_str(&format!("0x{}", hex::encode(bytes))), // Serialize Some bytes as a hexadecimal string with "0x" prefix
		None => Err(S::Error::custom("String serialize error.")), // Return an error if bytes are None
	}
}

// Serialize an opcode byte array as a string in uppercase
pub fn serialize_opcode<S>(opcode: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let d = std::str::from_utf8(opcode)
		.map_err(|_| S::Error::custom("Opcode serialize error."))?
		.to_uppercase(); // Convert opcode to string and uppercase
	serializer.serialize_str(&d) // Serialize the uppercase string
}

// Serialize a byte array as a UTF-8 string
pub fn serialize_string<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let d = std::str::from_utf8(value)
		.map_err(|_| S::Error::custom("String serialize error."))?
		.to_string(); // Convert byte array to UTF-8 string
	serializer.serialize_str(&d) // Serialize the UTF-8 string
}

// Serialize an optional byte array as a UTF-8 string
pub fn serialize_option_string<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	match value {
		Some(value) => {
			let d = std::str::from_utf8(&value[..])
				.map_err(|_| S::Error::custom("String serialize error."))?
				.to_string(); // Convert byte array to UTF-8 string
			serializer.serialize_str(&d) // Serialize the UTF-8 string
		}
		None => Err(S::Error::custom("String serialize error.")), // Return an error if value is None
	}
}

// Serialize a U256 value as a u64
pub fn serialize_u256<S>(data: &U256, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_u64(data.low_u64()) // Serialize the low 64 bits of the U256 value as a u64
}

// Serialize an H256 value as a hexadecimal string
pub fn serialize_h256<S>(data: &H256, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&format!("{:x}", data)) // Serialize the H256 value as a hexadecimal string
}

// Serialize an H256 value as a hexadecimal string with "0x" prefix
pub fn serialize_h256_0x<S>(data: &H256, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	serializer.serialize_str(&format!("0x{:x}", data)) // Serialize the H256 value as a hexadecimal string with "0x" prefix
}
