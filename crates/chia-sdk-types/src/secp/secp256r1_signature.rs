use clvm_traits::{ClvmDecoder, ClvmEncoder, FromClvm, FromClvmError, ToClvm, ToClvmError};
use clvmr::Atom;
use p256::ecdsa::{Error, Signature};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Secp256r1Signature(pub(crate) Signature);

impl Secp256r1Signature {
    pub const SIZE: usize = 64;

    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        self.0.to_bytes().into()
    }

    pub fn from_bytes(bytes: [u8; Self::SIZE]) -> Result<Self, Error> {
        Ok(Self(Signature::from_slice(&bytes)?))
    }
}

impl<E> ToClvm<E> for Secp256r1Signature
where
    E: ClvmEncoder,
{
    fn to_clvm(&self, encoder: &mut E) -> Result<E::Node, ToClvmError> {
        encoder.encode_atom(Atom::Borrowed(&self.0.to_vec()))
    }
}

impl<D> FromClvm<D> for Secp256r1Signature
where
    D: ClvmDecoder,
{
    fn from_clvm(decoder: &D, node: D::Node) -> Result<Self, FromClvmError> {
        let atom = decoder.decode_atom(&node)?;
        let bytes: [u8; Self::SIZE] =
            atom.as_ref()
                .try_into()
                .map_err(|_| FromClvmError::WrongAtomLength {
                    expected: Self::SIZE,
                    found: atom.len(),
                })?;
        Self::from_bytes(bytes).map_err(|error| FromClvmError::Custom(error.to_string()))
    }
}
