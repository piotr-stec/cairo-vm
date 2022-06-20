use std::fmt;

#[derive(Debug, PartialEq)]
pub enum MemoryError {
    UnallocatedSegment(usize, usize),
    AddressNotRelocatable,
    NumOutOfBounds,
    FoundNonInt,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MemoryError::UnallocatedSegment(len, accessed) => write!(
                f,
                "Can't insert into segment #{}; memory only has {} segment",
                accessed, len
            ),
            MemoryError::AddressNotRelocatable => write!(f, "Memory addresses must be relocatable"),
            MemoryError::NumOutOfBounds => write!(
                f,
                "Range-check validation failed, number is out of valid range"
            ),
            MemoryError::FoundNonInt => write!(
                f,
                "Range-check validation failed, encountered non-int value"
            ),
        }
    }
}