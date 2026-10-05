use std::collections::HashMap;

/// Nanoseconds since the Unix epoch (UTC).
pub type Nanos = u64;

/// Dense, process-local instrument id; indexes arrays in the hot path.
pub type InstrumentId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ProviderId {
    Synthetic = 0,
    Databento = 1,
    Alpaca = 2,
}

impl ProviderId {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(ProviderId::Synthetic),
            1 => Some(ProviderId::Databento),
            2 => Some(ProviderId::Alpaca),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderId::Synthetic => "synthetic",
            ProviderId::Databento => "databento",
            ProviderId::Alpaca => "alpaca",
        }
    }
}

/// Minimal symbol <-> id table. The persistent security master (stable ids
/// across days, ticker changes, corporate actions) is a backlog item; ids
/// here are only stable within one table.
#[derive(Clone, Debug, Default)]
pub struct SymbolTable {
    names: Vec<String>,
    by_name: HashMap<String, InstrumentId>,
}

impl SymbolTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn intern(&mut self, symbol: &str) -> InstrumentId {
        if let Some(&id) = self.by_name.get(symbol) {
            return id;
        }
        let id = InstrumentId::try_from(self.names.len()).expect("more than u32::MAX instruments");
        self.names.push(symbol.to_owned());
        self.by_name.insert(symbol.to_owned(), id);
        id
    }

    pub fn get(&self, symbol: &str) -> Option<InstrumentId> {
        self.by_name.get(symbol).copied()
    }

    pub fn name(&self, id: InstrumentId) -> Option<&str> {
        self.names.get(id as usize).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_is_idempotent_and_dense() {
        let mut t = SymbolTable::new();
        let a = t.intern("AAA");
        let b = t.intern("BBB");
        assert_eq!((a, b), (0, 1));
        assert_eq!(t.intern("AAA"), a);
        assert_eq!(t.name(b), Some("BBB"));
        assert_eq!(t.get("CCC"), None);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn provider_id_roundtrip() {
        for p in [
            ProviderId::Synthetic,
            ProviderId::Databento,
            ProviderId::Alpaca,
        ] {
            assert_eq!(ProviderId::from_u8(p.as_u8()), Some(p));
        }
        assert_eq!(ProviderId::from_u8(200), None);
    }
}
