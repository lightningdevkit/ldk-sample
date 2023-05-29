// This file was stolen from rust-crypto.
// Copyright 2012-2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use crate::io;

#[cfg(not(fuzzing))]
mod real_chacha {
	use core::cmp;
	use core::convert::TryInto;

	#[derive(Clone, Copy, PartialEq, Eq)]
	#[allow(non_camel_case_types)]
	struct u32x4(pub u32, pub u32, pub u32, pub u32);
	impl ::core::ops::Add for u32x4 {
		type Output = u32x4;
		fn add(self, rhs: u32x4) -> u32x4 {
			u32x4(self.0.wrapping_add(rhs.0),
			      self.1.wrapping_add(rhs.1),
			      self.2.wrapping_add(rhs.2),
			      self.3.wrapping_add(rhs.3))
		}
	}
	impl ::core::ops::Sub for u32x4 {
		type Output = u32x4;
		fn sub(self, rhs: u32x4) -> u32x4 {
			u32x4(self.0.wrapping_sub(rhs.0),
			      self.1.wrapping_sub(rhs.1),
			      self.2.wrapping_sub(rhs.2),
			      self.3.wrapping_sub(rhs.3))
		}
	}
	impl ::core::ops::BitXor for u32x4 {
		type Output = u32x4;
		fn bitxor(self, rhs: u32x4) -> u32x4 {
			u32x4(self.0 ^ rhs.0, self.1 ^ rhs.1, self.2 ^ rhs.2, self.3 ^ rhs.3)
		}
	}
	impl ::core::ops::Shr<u32x4> for u32x4 {
		type Output = u32x4;
		fn shr(self, rhs: u32x4) -> u32x4 {
			u32x4(self.0 >> rhs.0, self.1 >> rhs.1, self.2 >> rhs.2, self.3 >> rhs.3)
		}
	}
	impl ::core::ops::Shl<u32x4> for u32x4 {
		type Output = u32x4;
		fn shl(self, rhs: u32x4) -> u32x4 {
			u32x4(self.0 << rhs.0, self.1 << rhs.1, self.2 << rhs.2, self.3 << rhs.3)
		}
	}
	impl u32x4 {
		fn from_bytes(bytes: &[u8]) -> Self {
			assert_eq!(bytes.len(), 4*4);
			Self (
				u32::from_le_bytes(bytes[0*4..1*4].try_into().expect("len is 4")),
				u32::from_le_bytes(bytes[1*4..2*4].try_into().expect("len is 4")),
				u32::from_le_bytes(bytes[2*4..3*4].try_into().expect("len is 4")),
				u32::from_le_bytes(bytes[3*4..4*4].try_into().expect("len is 4")),
			)
		}
	}

	const BLOCK_SIZE: usize = 64;

	#[derive(Clone,Copy)]
	struct ChaChaState {
		a: u32x4,
		b: u32x4,
		c: u32x4,
		d: u32x4
	}

	#[derive(Copy)]
	pub struct ChaCha20 {
		state  : ChaChaState,
		output : [u8; BLOCK_SIZE],
		offset : usize,
	}

	impl Clone for ChaCha20 { fn clone(&self) -> ChaCha20 { *self } }

	macro_rules! swizzle {
		($b: expr, $c: expr, $d: expr) => {{
			let u32x4(b10, b11, b12, b13) = $b;
			$b = u32x4(b11, b12, b13, b10);
			let u32x4(c10, c11, c12, c13) = $c;
			$c = u32x4(c12, c13,c10, c11);
			let u32x4(d10, d11, d12, d13) = $d;
			$d = u32x4(d13, d10, d11, d12);
		}}
	}

	macro_rules! state_to_buffer {
		($state: expr, $output: expr) => {{
			let u32x4(a1, a2, a3, a4) = $state.a;
			let u32x4(b1, b2, b3, b4) = $state.b;
			let u32x4(c1, c2, c3, c4) = $state.c;
			let u32x4(d1, d2, d3, d4) = $state.d;
			let lens = [
				a1,a2,a3,a4,
				b1,b2,b3,b4,
				c1,c2,c3,c4,
				d1,d2,d3,d4
			];
			for i in 0..lens.len() {
				$output[i*4..(i+1)*4].copy_from_slice(&lens[i].to_le_bytes());
			}
		}}
	}

	macro_rules! round{
		($state: expr) => {{
			$state.a = $state.a + $state.b;
			rotate!($state.d, $state.a, S16);
			$state.c = $state.c + $state.d;
			rotate!($state.b, $state.c, S12);
			$state.a = $state.a + $state.b;
			rotate!($state.d, $state.a, S8);
			$state.c = $state.c + $state.d;
			rotate!($state.b, $state.c, S7);
		}}
	}

	macro_rules! rotate {
		($a: expr, $b: expr, $c:expr) => {{
			let v = $a ^ $b;
			let r = S32 - $c;
			let right = v >> r;
			$a = (v << $c) ^ right
		}}
	}

	const S32:u32x4 = u32x4(32, 32, 32, 32);
	const S16:u32x4 = u32x4(16, 16, 16, 16);
	const S12:u32x4 = u32x4(12, 12, 12, 12);
	const S8:u32x4 = u32x4(8, 8, 8, 8);
	const S7:u32x4 = u32x4(7, 7, 7, 7);

	impl ChaCha20 {
		pub fn new(key: &[u8], nonce: &[u8]) -> ChaCha20 {
			assert!(key.len() == 16 || key.len() == 32);
			assert!(nonce.len() == 8 || nonce.len() == 12);

			ChaCha20{ state: ChaCha20::expand(key, nonce), output: [0u8; BLOCK_SIZE], offset: 64 }
		}

		/// Get one block from a ChaCha stream.
		pub fn get_single_block(key: &[u8; 32], nonce: &[u8; 16]) -> [u8; 32] {
			let mut chacha = ChaCha20 { state: ChaCha20::expand(key, nonce), output: [0u8; BLOCK_SIZE], offset: 64 };
			let mut chacha_bytes = [0; 32];
			chacha.process_in_place(&mut chacha_bytes);
			chacha_bytes
		}

		fn expand(key: &[u8], nonce: &[u8]) -> ChaChaState {
			let constant = match key.len() {
				16 => b"expand 16-byte k",
				32 => b"expand 32-byte k",
				_  => unreachable!(),
			};
			ChaChaState {
				a: u32x4::from_bytes(&constant[0..16]),
				b: u32x4::from_bytes(&key[0..16]),
				c: if key.len() == 16 {
					u32x4::from_bytes(&key[0..16])
				} else {
					u32x4::from_bytes(&key[16..32])
				},
				d: if nonce.len() == 16 {
					u32x4::from_bytes(&nonce[0..16])
				} else if nonce.len() == 12 {
					let mut nonce4 = [0; 4*4];
					nonce4[4..].copy_from_slice(nonce);
					u32x4::from_bytes(&nonce4)
				} else {
					let mut nonce4 = [0; 4*4];
					nonce4[8..].copy_from_slice(nonce);
					u32x4::from_bytes(&nonce4)
				}
			}
		}

		// put the the next BLOCK_SIZE keystream bytes into self.output
		fn update(&mut self) {
			let mut state = self.state;

			for _ in 0..10 {
				round!(state);
				swizzle!(state.b, state.c, state.d);
				round!(state);
				swizzle!(state.d, state.c, state.b);
			}
			state.a = state.a + self.state.a;
			state.b = state.b + self.state.b;
			state.c = state.c + self.state.c;
			state.d = state.d + self.state.d;

			state_to_buffer!(state, self.output);

			self.state.d = self.state.d + u32x4(1, 0, 0, 0);
			let u32x4(c12, _, _, _) = self.state.d;
			if c12 == 0 {
				// we could increment the other counter word with an 8 byte nonce
				// but other implementations like boringssl have this same
				// limitation
				panic!("counter is exhausted");
			}

			self.offset = 0;
		}

		#[inline] // Useful cause input may be 0s on stack that should be optimized out
		pub fn process(&mut self, input: &[u8], output: &mut [u8]) {
			assert!(input.len() == output.len());
			let len = input.len();
			let mut i = 0;
			while i < len {
				// If there is no keystream available in the output buffer,
				// generate the next block.
				if self.offset == BLOCK_SIZE {
					self.update();
				}

				// Process the min(available keystream, remaining input length).
				let count = cmp::min(BLOCK_SIZE - self.offset, len - i);
				// explicitly assert lengths to avoid bounds checks:
				assert!(output.len() >= i + count);
				assert!(input.len() >= i + count);
				assert!(self.output.len() >= self.offset + count);
				for j in 0..count {
					output[i + j] = input[i + j] ^ self.output[self.offset + j];
				}
				i += count;
				self.offset += count;
			}
		}

		pub fn process_in_place(&mut self, input_output: &mut [u8]) {
			let len = input_output.len();
			let mut i = 0;
			while i < len {
				// If there is no keystream available in the output buffer,
				// generate the next block.
				if self.offset == BLOCK_SIZE {
					self.update();
				}

				// Process the min(available keystream, remaining input length).
				let count = cmp::min(BLOCK_SIZE - self.offset, len - i);
				// explicitly assert lengths to avoid bounds checks:
				assert!(input_output.len() >= i + count);
				assert!(self.output.len() >= self.offset + count);
				for j in 0..count {
					input_output[i + j] ^= self.output[self.offset + j];
				}
				i += count;
				self.offset += count;
			}
		}

		#[cfg(test)]
		pub fn seek_to_block(&mut self, block_offset: u32) {
			self.state.d.0 = block_offset;
			self.update();
		}
	}
}
#[cfg(not(fuzzing))]
pub use self::real_chacha::ChaCha20;

#[cfg(fuzzing)]
mod fuzzy_chacha {
	pub struct ChaCha20 {}

	impl ChaCha20 {
		pub fn new(key: &[u8], nonce: &[u8]) -> ChaCha20 {
			assert!(key.len() == 16 || key.len() == 32);
			assert!(nonce.len() == 8 || nonce.len() == 12);
			Self {}
		}

		pub fn get_single_block(_key: &[u8; 32], _nonce: &[u8; 16]) -> [u8; 32] {
			[0; 32]
		}

		pub fn process(&mut self, input: &[u8], output: &mut [u8]) {
			output.copy_from_slice(input);
		}

		pub fn process_in_place(&mut self, _input_output: &mut [u8]) {}
	}
}
#[cfg(fuzzing)]
pub use self::fuzzy_chacha::ChaCha20;

pub(crate) struct ChaChaReader<'a, R: io::Read> {
	pub chacha: &'a mut ChaCha20,
	pub read: R,
}
impl<'a, R: io::Read> io::Read for ChaChaReader<'a, R> {
	fn read(&mut self, dest: &mut [u8]) -> Result<usize, io::Error> {
		let res = self.read.read(dest)?;
		if res > 0 {
			self.chacha.process_in_place(&mut dest[0..res]);
		}
		Ok(res)
	}
}
