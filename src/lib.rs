// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs;
use std::io::Result;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaBuilder};
use datafusion::error::DataFusionError;
use datafusion::parquet::basic::Compression;
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::prelude::*;

pub mod tpcds;
pub mod tpch;

#[async_trait]
pub trait Tpc {
    fn generate(
        &self,
        scale: usize,
        partitions: usize,
        input_path: &str,
        output_path: &str,
    ) -> Result<()>;

    fn get_table_names(&self) -> Vec<&str>;

    fn get_table_ext(&self) -> &str;

    fn get_schema(&self, table: &str) -> Schema;
}

pub async fn convert_to_parquet(
    benchmark: &dyn Tpc,
    input_path: &str,
    output_path: &str,
) -> datafusion::error::Result<()> {
    for table in benchmark.get_table_names() {
        println!("Converting table {}", table);

        let mut schema_builder = SchemaBuilder::from(benchmark.get_schema(table).fields);
        schema_builder.push(Field::new("__placeholder", DataType::Utf8, true));
        let schema = schema_builder.finish();

        let file_ext = format!(".{}", benchmark.get_table_ext());
        let options = CsvReadOptions::new()
            .schema(&schema)
            .has_header(false)
            .delimiter(b'|')
            .file_extension(&file_ext);

        let path = format!("{}/{}.{}", input_path, table, benchmark.get_table_ext());
        let path = Path::new(&path);
        if !path.exists() {
            panic!("path does not exist: {:?}", path);
        }

        // create output dir
        let output_dir_name = format!("{}/{}.parquet", output_path, table);
        let output_dir = Path::new(&output_dir_name);
        if output_dir.exists() {
            panic!("output dir already exists: {}", output_dir.display());
        }
        println!("Creating directory: {}", output_dir.display());
        fs::create_dir(&output_dir)?;

        let x = PathBuf::from(path);
        let mut file_vec = vec![];
        if x.is_dir() {
            let files = fs::read_dir(path)?;
            for file in files {
                let file = file?;
                file_vec.push(file);
            }
        }

        let mut part = 0;
        for file in &file_vec {
            let stub = file.file_name().to_str().unwrap().to_owned();
            let stub = &stub[0..stub.len() - 4]; // remove .dat or .tbl
                                                 // write to temp dir that will contain nested dirs
                                                 // example: /tmp/nation-temp.parquet/part-1.parquet/part-0.parquet
            let output_parts_dir = format!("{}/{}-temp.parquet", output_dir.display(), stub);
            println!("Writing {}", output_parts_dir);
            let options = options.clone();
            // async move {
            convert_tbl(
                &file.path(),
                &output_parts_dir,
                &options,
                "parquet",
                "snappy",
                8192,
            )
            .await?;
            // }

            let paths = fs::read_dir(&output_parts_dir)?;
            for path in paths {
                let path = path?;
                let dest_file = format!("{}/part-{}.parquet", output_dir.display(), part);
                part += 1;
                let dest_path = Path::new(&dest_file);
                move_or_copy(&path.path(), &dest_path)?;
            }
            println!("Removing {}", output_parts_dir);
            fs::remove_dir_all(Path::new(&output_parts_dir))?;
        }
    }

    Ok(())
}

pub(crate) fn move_or_copy(
    source_path: &Path,
    dest_path: &Path,
) -> std::result::Result<(), std::io::Error> {
    if is_same_device(&source_path, &dest_path)? {
        println!(
            "Moving {} to {}",
            source_path.display(),
            dest_path.display()
        );
        fs::rename(&source_path, &dest_path)
    } else {
        println!(
            "Copying {} to {}",
            source_path.display(),
            dest_path.display()
        );
        fs::copy(&source_path, &dest_path)?;
        fs::remove_file(&source_path)
    }
}

#[cfg(unix)]
fn is_same_device(path1: &Path, path2: &Path) -> std::result::Result<bool, std::io::Error> {
    use std::os::unix::fs::MetadataExt;
    let meta1 = fs::metadata(path1)?;
    let meta2 = fs::metadata(path2.parent().unwrap())?;
    Ok(meta1.dev() == meta2.dev())
}

#[cfg(windows)]
fn is_same_device(path1: &Path, path2: &Path) -> std::result::Result<bool, std::io::Error> {
    use std::os::windows::fs::MetadataExt;
    let meta1 = fs::metadata(path1)?;
    let meta2 = fs::metadata(path2.parent().unwrap())?;
    Ok(meta1.volume_serial_number() == meta2.volume_serial_number())
}

pub async fn convert_tbl(
    input_path: &Path,
    output_filename: &str,
    options: &CsvReadOptions<'_>,
    file_format: &str,
    compression: &str,
    batch_size: usize,
) -> datafusion::error::Result<()> {
    println!(
        "Converting '{}' to {}",
        input_path.display(),
        output_filename
    );

    let start = Instant::now();

    let config = SessionConfig::new().with_batch_size(batch_size);
    let ctx = SessionContext::with_config(config);

    // build plan to read the TBL file
    let csv_filename = format!("{}", input_path.display());
    let mut df = ctx.read_csv(&csv_filename, options.clone()).await?;

    let schema = df.schema();
    // Select all apart from the padding column
    let selection = df
        .schema()
        .fields()
        .iter()
        .take(schema.fields().len() - 1)
        .map(|d| Expr::Column(d.qualified_column()))
        .collect();

    df = df.select(selection)?;

    match file_format {
        "csv" => df.write_csv(&output_filename).await?,
        "parquet" => {
            let compression = match compression {
                "none" => Compression::UNCOMPRESSED,
                "snappy" => Compression::SNAPPY,
                // "brotli" => Compression::BROTLI,
                // "gzip" => Compression::GZIP,
                "lz4" => Compression::LZ4,
                "lz0" => Compression::LZO,
                // "zstd" => Compression::ZSTD,
                other => {
                    return Err(DataFusionError::NotImplemented(format!(
                        "Invalid compression format: {}",
                        other
                    )))
                }
            };
            let props = WriterProperties::builder()
                .set_compression(compression)
                .build();

            df.write_parquet(&output_filename, Some(props)).await?
        }
        other => {
            return Err(DataFusionError::NotImplemented(format!(
                "Invalid output format: {}",
                other
            )))
        }
    }
    println!("Conversion completed in {} ms", start.elapsed().as_millis());

    Ok(())
}
