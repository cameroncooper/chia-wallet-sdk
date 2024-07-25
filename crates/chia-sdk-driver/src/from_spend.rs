use std::string::ParseError;

use chia_protocol::CoinSpend;
use clvmr::Allocator;

// given a spend, will return info about the coin being spent
pub trait FromSpend<R, A> {
    fn from_spend(
        allocator: &mut Allocator,
        cs: &CoinSpend,
        additional_info: A,
    ) -> Result<R, ParseError>;
}
