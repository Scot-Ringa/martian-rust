//! A delimited file such as a csv file or a tab file stores a list of
//! items of type `T`.

use crate::{FileStorage, FileTypeIO};
use martian::{Error, MartianFileType};
use martian_derive::martian_filetype;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::io::{Read, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

pub trait Delimiter {
    fn delimiter() -> u8;
    fn format() -> String;
    fn header() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct DelimitedFormat<F, D>
where
    F: MartianFileType,
    D: Delimiter + Debug,
{
    path: PathBuf,
    #[serde(skip)]
    phantom: PhantomData<(F, D)>,
}

impl<F, D> MartianFileType for DelimitedFormat<F, D>
where
    F: MartianFileType,
    D: Delimiter + Debug,
{
    fn extension() -> String {
        if F::extension().ends_with(&D::format()) || D::format() == "" {
            F::extension()
        } else {
            format!("{}.{}", F::extension(), D::format())
        }
    }

    fn new(file_path: impl AsRef<std::path::Path>, file_name: impl AsRef<std::path::Path>) -> Self {
        let mut path = std::path::PathBuf::from(file_path.as_ref());
        path.push(file_name);
        let path = ::martian::utils::set_extension(path, Self::extension());
        DelimitedFormat {
            phantom: ::std::marker::PhantomData,
            path,
        }
    }
}

impl<F, D> AsRef<Path> for DelimitedFormat<F, D>
where
    F: MartianFileType,
    D: Delimiter + Debug,
{
    fn as_ref(&self) -> &Path {
        self.path.as_ref()
    }
}

impl<F, D, T> FileStorage<Vec<T>> for DelimitedFormat<F, D>
where
    F: MartianFileType + FileStorage<Vec<T>>,
    D: Delimiter + Debug,
{
}

#[derive(Debug)]
pub struct CommaDelimiter;
impl Delimiter for CommaDelimiter {
    fn delimiter() -> u8 {
        b','
    }
    fn format() -> String {
        "csv".into()
    }
}
pub type CsvFormat<F> = DelimitedFormat<F, CommaDelimiter>;
martian_filetype! {Csv, "csv"}
impl<T> FileStorage<Vec<T>> for Csv where T: Serialize + DeserializeOwned {}
pub type CsvFile = CsvFormat<Csv>;

#[derive(Debug)]
pub struct TabDelimiter;
impl Delimiter for TabDelimiter {
    fn delimiter() -> u8 {
        b'\t'
    }
    fn format() -> String {
        "tsv".into()
    }
}
pub type TsvFormat<F> = DelimitedFormat<F, TabDelimiter>;
martian_filetype! {Tsv, "tsv"}
impl<T> FileStorage<Vec<T>> for Tsv where T: Serialize + DeserializeOwned {}
pub type TsvFile = TsvFormat<Tsv>;

/// Any type `T` that can be deserialized implements `read()` from a `JsonFile`
/// Any type `T` that can be serialized can be saved as a `JsonFile`.
/// The saved JsonFile will be pretty formatted using 4 space indentation.
impl<F, D, T> FileTypeIO<Vec<T>> for DelimitedFormat<F, D>
where
    T: Serialize + DeserializeOwned,
    F: MartianFileType + FileStorage<Vec<T>> + Debug,
    D: Delimiter + Debug,
{
    fn read_from<R: Read>(reader: R) -> Result<Vec<T>, Error> {
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(D::delimiter())
            .has_headers(D::header())
            .from_reader(reader);
        let iter = rdr.deserialize::<T>();
        let rows = iter.collect::<csv::Result<Vec<T>>>()?;
        Ok(rows)
    }

    fn write_into<W: Write>(writer: W, item: &Vec<T>) -> Result<(), Error> {
        let mut wtr = csv::WriterBuilder::default()
            .delimiter(D::delimiter())
            .has_headers(D::header())
            .from_writer(writer);

        for d in item {
            wtr.serialize(d)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize, Deserialize, PartialEq)]
    struct Cell {
        barcode: String,
        genome: String,
    }

    fn cells() -> Vec<Cell> {
        vec![
            Cell {
                barcode: "ACGT".to_string(),
                genome: "hg19".to_string(),
            },
            Cell {
                barcode: "TCAT".to_string(),
                genome: "mm10".to_string(),
            },
        ]
    }

    #[test]
    fn test_csv_write() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let cells_csv = CsvFile::new(dir.path(), "test");
        cells_csv.write(&cells())?;
        assert_eq!(
            std::fs::read_to_string(&cells_csv)?,
            "barcode,genome\nACGT,hg19\nTCAT,mm10\n"
        );
        Ok(())
    }

    #[test]
    fn test_tsv_write() -> Result<(), Error> {
        let dir = tempfile::tempdir()?;
        let cells_tsv = TsvFile::new(dir.path(), "test");
        cells_tsv.write(&cells())?;
        assert_eq!(
            std::fs::read_to_string(&cells_tsv)?,
            "barcode\tgenome\nACGT\thg19\nTCAT\tmm10\n"
        );
        Ok(())
    }

    #[test]
    fn test_round_trip() -> Result<(), Error> {
        assert!(crate::round_trip_check::<CsvFile, _>(&cells())?);
        assert!(crate::round_trip_check::<TsvFile, _>(&cells())?);
        Ok(())
    }
}