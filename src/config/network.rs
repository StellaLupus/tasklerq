//! Сеть CIDR для списка доверенных прокси консоли.

use std::fmt;
use std::net::IpAddr;

/// Сеть CIDR: адрес и длина префикса.
///
/// Адрес без `/n` считается сетью из одного адреса (`/32` для IPv4 и `/128`
/// для IPv6). Адрес с префиксом не маскируется при разборе: [`IpNet::contains`]
/// сравнивает только первые `prefix` бит, поэтому `10.1.2.3/8` и `10.0.0.0/8`
/// равнозначны.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Разобрать сеть вида `10.0.0.0/8`, `2001:db8::/32` или одиночный адрес.
    ///
    /// Причина ошибки не содержит исходное значение: список приходит из
    /// окружения и может оказаться секретом по ошибке оператора.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        let (addr_text, prefix_text) = match raw.split_once('/') {
            Some((addr, prefix)) => (addr.trim(), Some(prefix.trim())),
            None => (raw, None),
        };

        let addr = addr_text
            .parse::<IpAddr>()
            .map_err(|_| "expected an IPv4 or IPv6 network like 10.0.0.0/8".to_owned())?;
        let max_prefix = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };

        let prefix = match prefix_text {
            None => max_prefix,
            Some(text) => {
                let value = text
                    .parse::<u8>()
                    .map_err(|_| format!("expected a prefix length 0..={max_prefix}"))?;
                if value > max_prefix {
                    return Err(format!("expected a prefix length 0..={max_prefix}"));
                }
                value
            }
        };

        Ok(Self { addr, prefix })
    }

    /// Адрес сети; хвостовые биты за пределами префикса не обнуляются.
    pub const fn addr(self) -> IpAddr {
        self.addr
    }

    /// Длина префикса в битах.
    pub const fn prefix_len(self) -> u8 {
        self.prefix
    }

    /// Входит ли адрес в сеть. Сети разного семейства не пересекаются.
    ///
    /// Проверяемый адрес приводится через [`IpAddr::to_canonical`]: пир
    /// `::ffff:10.0.0.1` на dual-stack-сокете входит в `10.0.0.0/8`.
    pub fn contains(self, other: IpAddr) -> bool {
        match (self.addr, other.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(address)) => {
                let different = u32::from(network) ^ u32::from(address);
                different.leading_zeros() >= u32::from(self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(address)) => {
                let different = u128::from(network) ^ u128::from(address);
                different.leading_zeros() >= u32::from(self.prefix)
            }
            _ => false,
        }
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.addr, self.prefix)
    }
}
