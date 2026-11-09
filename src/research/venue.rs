// Copyright (c) 2024-2026 ScreenerBot (screenerbot.io)
// SPDX-License-Identifier: BUSL-1.1

//! Where a mint's pool sits. Pump.fun bonding curves are part of the universe,
//! not only pools that have graduated onto a standard AMM.

/// The venue class stored on a mint and a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Venue {
    /// Unknown until a source names a DEX.
    Unknown,
    /// Pump.fun bonding curve. The mint may never graduate.
    BondingCurve,
    /// A graduated or ordinary AMM pool, including PumpSwap.
    GraduatedAmm,
    /// Some other indexed pool.
    Other,
}

impl Venue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::BondingCurve => "bonding_curve",
            Self::GraduatedAmm => "graduated_amm",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "bonding_curve" => Self::BondingCurve,
            "graduated_amm" => Self::GraduatedAmm,
            "other" => Self::Other,
            _ => Self::Unknown,
        }
    }
}

/// Map a free-source DEX id onto a venue. Unrecognized ids stay `Other` when
/// the source did name a DEX, and `Unknown` when it did not.
pub fn venue_from_dex_id(dex_id: &str) -> Venue {
    let id = dex_id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return Venue::Unknown;
    }
    if matches!(
        id.as_str(),
        "pump-fun" | "pumpfun" | "pump_fun" | "pump.fun"
    ) {
        return Venue::BondingCurve;
    }
    if id.contains("pump") && (id.contains("swap") || id.contains("amm")) {
        return Venue::GraduatedAmm;
    }
    Venue::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pumpfun_bonding_curves_are_not_treated_as_graduated_pools() {
        assert_eq!(venue_from_dex_id("pump-fun"), Venue::BondingCurve);
        assert_eq!(venue_from_dex_id("pumpfun"), Venue::BondingCurve);
        assert_eq!(venue_from_dex_id("pumpswap"), Venue::GraduatedAmm);
        assert_eq!(venue_from_dex_id("pump-amm"), Venue::GraduatedAmm);
        assert_eq!(venue_from_dex_id("raydium"), Venue::Other);
        assert_eq!(venue_from_dex_id(""), Venue::Unknown);
    }
}
