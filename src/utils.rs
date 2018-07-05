use bitcoin::util::hash::Sha256dHash;

pub fn hex_to_vec(hex: &str) -> Option<Vec<u8>> {
	let mut out = Vec::with_capacity(hex.len() / 2);

	let mut b = 0;
	for (idx, c) in hex.as_bytes().iter().enumerate() {
		b <<= 4;
		match *c {
			b'A'...b'F' => b |= c - b'A' + 10,
			b'a'...b'f' => b |= c - b'a' + 10,
			b'0'...b'9' => b |= c - b'0',
			_ => return None,
		}
		if (idx & 1) == 1 {
			out.push(b);
			b = 0;
		}
	}

	Some(out)
}

pub fn hex_to_u256_rev(hex: &str) -> Option<Sha256dHash> {
	if hex.len() != 64 { return None; }

	let mut out = [0; 32];

	let mut b = 0;
	let mut outpos = 32;
	for (idx, c) in hex.as_bytes().iter().enumerate() {
		b <<= 4;
		match *c {
			b'A'...b'F' => b |= c - b'A' + 10,
			b'a'...b'f' => b |= c - b'a' + 10,
			b'0'...b'9' => b |= c - b'0',
			_ => return None,
		}
		if (idx & 1) == 1 {
			outpos -= 1;
			out[outpos] = b;
			b = 0;
		}
	}

	Some(Sha256dHash::from_data(&out))
}

