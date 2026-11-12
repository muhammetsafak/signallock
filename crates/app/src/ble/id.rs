//! `DeviceId`: the platform-neutral string identity of a BLE device.
//!
//! The string is what `signallock-core`'s `TrustedDevice.identifier` stores,
//! so the formats are fixed by the legacy apps:
//!
//! - **macOS:** the CoreBluetooth peripheral UUID, upper case with hyphens
//!   (`Foundation.UUID.uuidString`), e.g. `E621E1F8-C36C-495A-93FC-0C247A3E6E5F`.
//! - **Windows and Linux:** the 48-bit Bluetooth address as 12 upper-case hex
//!   digits without separators (`TrustedDevice.FormatAddress`, C# `"X12"`),
//!   e.g. `0123456789AB`.
//!
//! `DeviceId::from_str` is lenient on input (case, `:`/`-` separated
//! addresses) and normalises to the canonical form, so an identifier read from
//! a hand-edited or legacy file still compares equal to a freshly scanned one.
//! Building one never touches a platform API; the btleplug mapping lives in
//! `btleplug_scanner.rs`.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

/// Largest value a 48-bit Bluetooth address can take.
const MAX_ADDRESS: u64 = 0xFFFF_FFFF_FFFF;

/// Canonical, normalised device identity. See the module docs for the formats.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId(String);

/// The string was neither a UUID nor a 48-bit Bluetooth address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseDeviceIdError;

impl fmt::Display for ParseDeviceIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not a UUID and not a 12-digit Bluetooth address")
    }
}

impl std::error::Error for ParseDeviceIdError {}

impl DeviceId {
    /// CoreBluetooth identity: upper-case hyphenated UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid.hyphenated().to_string().to_uppercase())
    }

    /// Bluetooth address from its six bytes in display order
    /// (`[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB]` is `01:23:45:67:89:AB`).
    pub fn from_address_bytes(bytes: [u8; 6]) -> Self {
        let mut id = String::with_capacity(12);
        for byte in bytes {
            id.push_str(&format!("{byte:02X}"));
        }
        Self(id)
    }

    /// Bluetooth address packed into an integer, as the Windows reference
    /// does (`ulong`). `None` when the value does not fit in 48 bits.
    pub fn from_address(address: u64) -> Option<Self> {
        if address > MAX_ADDRESS {
            return None;
        }
        Some(Self(format!("{address:012X}")))
    }

    /// The canonical string, as stored in `trusted-device.json`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `Some` when this is a CoreBluetooth UUID identity.
    pub fn to_uuid(&self) -> Option<Uuid> {
        if self.0.len() == 36 {
            Uuid::parse_str(&self.0).ok()
        } else {
            None
        }
    }

    /// `Some` when this is a Bluetooth address identity (the Windows
    /// reference's `TryParseAddress`).
    pub fn to_address(&self) -> Option<u64> {
        if self.0.len() == 12 {
            u64::from_str_radix(&self.0, 16).ok()
        } else {
            None
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for DeviceId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl FromStr for DeviceId {
    type Err = ParseDeviceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();

        // Hyphenated UUID (36 chars). `Uuid::parse_str` also accepts the
        // simple, braced and URN forms; only the hyphenated one is a legacy
        // format, so the length is checked first.
        if s.len() == 36 {
            return Uuid::parse_str(s)
                .map(Self::from_uuid)
                .map_err(|_| ParseDeviceIdError);
        }

        // Address: 12 hex digits, optionally as six `:` or `-` separated pairs.
        let digits = if s.len() == 17 && is_pair_separated(s) {
            s.chars()
                .filter(|c| *c != ':' && *c != '-')
                .collect::<String>()
        } else {
            s.to_owned()
        };
        if digits.len() == 12 && digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(Self(digits.to_ascii_uppercase()));
        }
        Err(ParseDeviceIdError)
    }
}

/// `AA:BB:CC:DD:EE:FF` or `AA-BB-CC-DD-EE-FF` (one separator kind throughout).
fn is_pair_separated(s: &str) -> bool {
    let bytes = s.as_bytes();
    let sep = bytes[2];
    (sep == b':' || sep == b'-')
        && bytes.iter().enumerate().all(|(i, b)| {
            if i % 3 == 2 {
                *b == sep
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A CoreBluetooth identifier as the Swift app persists it.
    const MACOS_ID: &str = "E621E1F8-C36C-495A-93FC-0C247A3E6E5F";

    #[test]
    fn macos_uuid_round_trips_in_the_legacy_format() {
        let id: DeviceId = MACOS_ID.parse().unwrap();
        assert_eq!(id.as_str(), MACOS_ID);
        let uuid = id.to_uuid().unwrap();
        assert_eq!(DeviceId::from_uuid(uuid), id);
        assert_eq!(id.to_address(), None);
    }

    #[test]
    fn uuid_from_btleplug_display_is_upper_cased() {
        // btleplug's macOS `PeripheralId` displays a lower-case hyphenated UUID.
        let lower = MACOS_ID.to_lowercase();
        let id: DeviceId = lower.parse().unwrap();
        assert_eq!(id.as_str(), MACOS_ID);
        assert_eq!(
            DeviceId::from_uuid(Uuid::parse_str(&lower).unwrap()).as_str(),
            MACOS_ID
        );
    }

    #[test]
    fn windows_address_round_trips_in_the_legacy_format() {
        // `TrustedDevice.FormatAddress(0x0123456789AB)` is "0123456789AB":
        // upper case, zero padded to 12 digits.
        let id = DeviceId::from_address(0x0123_4567_89AB).unwrap();
        assert_eq!(id.as_str(), "0123456789AB");
        assert_eq!(id.to_address(), Some(0x0123_4567_89AB));
        assert_eq!("0123456789AB".parse::<DeviceId>().unwrap(), id);
        assert_eq!(id.to_uuid(), None);
    }

    #[test]
    fn leading_zeros_are_preserved() {
        let id = DeviceId::from_address(0xAB).unwrap();
        assert_eq!(id.as_str(), "0000000000AB");
        assert_eq!(id.to_address(), Some(0xAB));
    }

    #[test]
    fn address_bytes_and_integer_agree() {
        let from_bytes = DeviceId::from_address_bytes([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB]);
        assert_eq!(
            from_bytes,
            DeviceId::from_address(0x0123_4567_89AB).unwrap()
        );
        assert_eq!(
            DeviceId::from_address_bytes([0xFF; 6]).as_str(),
            "FFFFFFFFFFFF"
        );
    }

    #[test]
    fn address_input_is_normalised() {
        let canonical = "0123456789AB";
        for input in [
            "0123456789ab",
            "01:23:45:67:89:ab",
            "01:23:45:67:89:AB",
            "01-23-45-67-89-ab",
            "  0123456789AB \n",
        ] {
            assert_eq!(
                input.parse::<DeviceId>().unwrap().as_str(),
                canonical,
                "{input:?}"
            );
        }
    }

    #[test]
    fn address_above_48_bits_is_rejected() {
        assert!(DeviceId::from_address(0x1_0000_0000_0000).is_none());
        assert!(DeviceId::from_address(MAX_ADDRESS).is_some());
    }

    #[test]
    fn garbage_is_rejected() {
        for input in [
            "",
            "   ",
            "Unknown Device",
            "0123456789A",                      // 11 digits
            "0123456789ABC",                    // 13 digits
            "0123456789AG",                     // not hex
            "01:23:45:67:89",                   // five pairs
            "01:23-45:67:89:AB",                // mixed separators
            "E621E1F8C36C495A93FC0C247A3E6E5F", // simple UUID: not a legacy format
            "{E621E1F8-C36C-495A-93FC-0C247A3E6E5F}",
            "E621E1F8-C36C-495A-93FC-0C247A3E6E5Z",
        ] {
            assert!(
                input.parse::<DeviceId>().is_err(),
                "{input:?} should be rejected"
            );
        }
    }

    #[test]
    fn a_value_re_read_from_disk_equals_a_freshly_scanned_one() {
        let stored: DeviceId = "e621e1f8-c36c-495a-93fc-0c247a3e6e5f".parse().unwrap();
        let scanned = DeviceId::from_uuid(Uuid::parse_str(MACOS_ID).unwrap());
        assert_eq!(stored, scanned);
        assert_eq!(stored.to_string(), MACOS_ID);
    }
}
