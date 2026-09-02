use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

#[derive(Debug)]
pub(crate) enum LimitedReadError {
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for LimitedReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

pub(crate) fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>, LimitedReadError> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    if length > limit as u64 {
        return Err(LimitedReadError::TooLarge);
    }

    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length as usize)
        .map_err(|error| io::Error::other(error.to_string()))?;
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(LimitedReadError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn read_file_enforces_the_limit_on_the_open_handle() {
        let mut file = tempfile::NamedTempFile::new().expect("temporary file");
        file.write_all(b"12345").expect("write test data");

        assert!(matches!(
            read_file(file.path(), 4),
            Err(LimitedReadError::TooLarge)
        ));
        assert_eq!(read_file(file.path(), 5).expect("bounded read"), b"12345");
    }
}
