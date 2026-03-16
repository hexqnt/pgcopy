use std::io::{Read, Write};
use std::path::Path;

use age::secrecy::SecretString;
use anyhow::{Context, Result, bail};

use crate::crypto;
use crate::manifest::{self, Manifest};

/// Информация о доступе к bundle: зашифрован ли файл и каким паролем его открыть.
#[derive(Debug, Clone)]
pub struct BundleAccess {
    pub is_encrypted: bool,
    pub password: Option<crypto::BundlePassword>,
}

/// Определяет режим доступа к bundle и проверяет обязательность пароля.
pub fn resolve_access(bundle_path: &Path, cli_password: Option<&str>) -> Result<BundleAccess> {
    let password = crypto::resolve_bundle_password(cli_password)?;
    let is_encrypted = crypto::is_age_encrypted_bundle(bundle_path)?;
    if is_encrypted && password.is_none() {
        bail!(
            "bundle {} is encrypted with age passphrase; provide --password or set PASSWORD",
            bundle_path.display()
        );
    }

    Ok(BundleAccess {
        is_encrypted,
        password,
    })
}

/// Открывает reader для payload bundle (дешифруя и распаковывая при необходимости).
pub fn open_bundle_reader(bundle_path: &Path, access: &BundleAccess) -> Result<Box<dyn Read>> {
    let input = std::fs::File::open(bundle_path)
        .with_context(|| format!("failed to open bundle {}", bundle_path.display()))?;

    if access.is_encrypted {
        let password = access.password.as_ref().with_context(|| {
            format!(
                "bundle {} is encrypted and requires --password or PASSWORD",
                bundle_path.display()
            )
        })?;
        let decryptor = age::Decryptor::new_buffered(std::io::BufReader::new(input))
            .with_context(|| format!("failed to read age header from {}", bundle_path.display()))?;
        if !decryptor.is_scrypt() {
            // В проекте поддерживается только парольная схема age (scrypt),
            // чтобы пользователю было достаточно одного passphrase.
            bail!(
                "unsupported encrypted bundle: expected passphrase-based age file, got recipient-based file"
            );
        }

        let identity = age::scrypt::Identity::new(SecretString::from(password.as_str().to_owned()));
        let decrypted = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .context("failed to decrypt bundle (wrong password?)")?;
        let decoder = zstd::stream::Decoder::new(decrypted).with_context(|| {
            format!(
                "failed to open zstd decoder for decrypted bundle {}",
                bundle_path.display()
            )
        })?;
        Ok(Box::new(decoder))
    } else {
        let decoder = zstd::stream::Decoder::new(input).with_context(|| {
            format!(
                "failed to open zstd decoder for {}; if this bundle is encrypted, use --password or PASSWORD",
                bundle_path.display()
            )
        })?;
        Ok(Box::new(decoder))
    }
}

/// Читает `manifest.json` напрямую из bundle без распаковки на диск.
pub fn read_manifest_from_bundle(bundle_path: &Path, access: &BundleAccess) -> Result<Manifest> {
    let reader = open_bundle_reader(bundle_path, access)?;
    let mut archive = tar::Archive::new(reader);
    let mut entries = archive
        .entries()
        .context("failed to enumerate bundle archive entries")?;
    read_manifest_from_entries(&mut entries)
}

/// Читает и валидирует `manifest.json` из stream tar-entries.
pub fn read_manifest_from_entries<R: Read>(entries: &mut tar::Entries<'_, R>) -> Result<Manifest> {
    let mut manifest_entry = next_required_entry(entries, "manifest.json")?;
    let mut manifest_raw = String::new();
    manifest_entry
        .read_to_string(&mut manifest_raw)
        .context("failed to read manifest.json from bundle")?;

    manifest::parse_manifest(&manifest_raw, "manifest.json")
}

/// Возвращает следующий обязательный entry и проверяет строгий порядок layout.
pub fn next_required_entry<'a, R: Read>(
    entries: &mut tar::Entries<'a, R>,
    expected_rel_path: &str,
) -> Result<tar::Entry<'a, R>> {
    let entry_result = entries.next().with_context(|| {
        format!("bundle is truncated: expected archive entry '{expected_rel_path}'")
    })?;
    let entry_result = entry_result.with_context(|| {
        format!("failed to read archive entry while expecting '{expected_rel_path}'")
    })?;
    let entry = entry_result;
    let actual_path = entry
        .path()
        .context("failed to resolve archive entry path")?
        .into_owned();
    if actual_path != Path::new(expected_rel_path) {
        bail!(
            "bundle layout error: expected archive entry '{}', got '{}'",
            expected_rel_path,
            actual_path.display()
        );
    }

    Ok(entry)
}

/// Распаковывает bundle в каталог `output_dir`.
pub fn unpack_bundle(bundle_path: &Path, output_dir: &Path, access: &BundleAccess) -> Result<()> {
    let reader = open_bundle_reader(bundle_path, access)?;
    let mut archive = tar::Archive::new(reader);
    archive
        .unpack(output_dir)
        .with_context(|| format!("failed to unpack bundle {}", bundle_path.display()))?;

    Ok(())
}

/// Читает и валидирует manifest из уже распакованной директории.
pub fn read_manifest_from_dir(root_dir: &Path) -> Result<Manifest> {
    let manifest_path = root_dir.join("manifest.json");
    let manifest_raw = std::fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read manifest file {}", manifest_path.display()))?;

    manifest::parse_manifest(&manifest_raw, &manifest_path.display().to_string())
}

/// Собирает bundle из scratch-структуры и при необходимости шифрует его.
pub fn write_bundle(
    scratch_dir: &Path,
    out_path: &Path,
    manifest: &Manifest,
    password: Option<&crypto::BundlePassword>,
) -> Result<()> {
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }

    let manifest_path = scratch_dir.join("manifest.json");
    let manifest_json = serde_json::to_vec_pretty(manifest)?;
    std::fs::write(&manifest_path, manifest_json)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    let output_file = std::fs::File::create(out_path)
        .with_context(|| format!("failed to create bundle file {}", out_path.display()))?;

    if let Some(password) = password {
        let encryptor =
            age::Encryptor::with_user_passphrase(SecretString::from(password.as_str().to_owned()));
        let encrypted_writer = encryptor
            .wrap_output(output_file)
            .context("failed to initialize age encryptor for bundle")?;
        let encrypted_writer =
            write_bundle_archive(encrypted_writer, scratch_dir, &manifest_path, manifest)?;
        encrypted_writer
            .finish()
            .context("failed to finalize encrypted bundle stream")?;
    } else {
        let _ = write_bundle_archive(output_file, scratch_dir, &manifest_path, manifest)?;
    }

    Ok(())
}

fn write_bundle_archive<W: Write>(
    writer: W,
    scratch_dir: &Path,
    manifest_path: &Path,
    manifest: &Manifest,
) -> Result<W> {
    // В архив пишем manifest первым, чтобы его можно было читать потоково
    // без распаковки всего bundle.
    let mut encoder =
        zstd::stream::Encoder::new(writer, 3).context("failed to initialize zstd encoder")?;

    {
        let mut archive = tar::Builder::new(&mut encoder);
        archive
            .append_path_with_name(manifest_path, "manifest.json")
            .context("failed to append manifest.json to bundle")?;

        // Формат v2: сначала все DDL, затем все data.
        // Это позволяет `--ddl-only` завершаться без чтения payload data/*.
        for object in &manifest.objects {
            append_entry(&mut archive, scratch_dir, &object.ddl_path)?;
        }
        for object in &manifest.objects {
            append_entry(&mut archive, scratch_dir, &object.data_path)?;
        }

        archive.finish().context("failed to finalize bundle tar")?;
    }

    encoder
        .finish()
        .context("failed to finalize zstd stream for bundle")
}

fn append_entry<W: Write>(
    archive: &mut tar::Builder<W>,
    base_dir: &Path,
    rel_path: &str,
) -> Result<()> {
    let abs_path = base_dir.join(rel_path);
    archive
        .append_path_with_name(&abs_path, rel_path)
        .with_context(|| format!("failed to append {rel_path} to bundle"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{BundleAccess, open_bundle_reader, write_bundle};
    use crate::manifest::{Manifest, ManifestObject};
    use crate::pg::RelationKind;
    use crate::select_dsl::ProjectionKind;
    use crate::types::{DataFormat, ExportAs};

    fn test_object(source_name: &str, index: usize) -> ManifestObject {
        let stem = format!("{:04}__public.{source_name}", index + 1);
        ManifestObject {
            kind: RelationKind::Table,
            export_as: ExportAs::Table,
            source_schema: "public".to_owned(),
            source_name: source_name.to_owned(),
            target_schema: "archive".to_owned(),
            target_name: source_name.to_owned(),
            source_select: format!("select * from public.{source_name}"),
            normalized_select: format!("SELECT \"id\" FROM \"public\".\"{source_name}\""),
            ddl_path: format!("ddl/{stem}.sql"),
            data_path: format!("data/{stem}.copybin"),
            effective_columns: vec!["id".to_owned()],
            effective_column_types: vec!["bigint".to_owned()],
            column_projection: ProjectionKind::All,
            row_estimate: Some(1),
        }
    }

    #[test]
    fn writes_v2_layout_with_grouped_ddl_then_data() {
        let scratch = tempfile::tempdir().expect("tempdir must be created");
        fs::create_dir_all(scratch.path().join("ddl")).expect("ddl dir must be created");
        fs::create_dir_all(scratch.path().join("data")).expect("data dir must be created");

        let object_a = test_object("orders", 0);
        let object_b = test_object("payments", 1);
        for object in [&object_a, &object_b] {
            fs::write(scratch.path().join(&object.ddl_path), "-- ddl")
                .expect("ddl file must be written");
            fs::write(scratch.path().join(&object.data_path), [0_u8, 1_u8, 2_u8])
                .expect("data file must be written");
        }

        let manifest = Manifest {
            format_version: 2,
            created_at: "2026-03-02T12:00:00Z".to_owned(),
            source_fingerprint: Some("database=app user=app".to_owned()),
            source_pg_version_num: 150002,
            data_format: DataFormat::Binary,
            consistent_snapshot: true,
            objects: vec![object_a.clone(), object_b.clone()],
        };

        let bundle_path = scratch.path().join("bundle.tar.zst");
        write_bundle(scratch.path(), &bundle_path, &manifest, None)
            .expect("bundle must be written");

        let reader = open_bundle_reader(
            &bundle_path,
            &BundleAccess {
                is_encrypted: false,
                password: None,
            },
        )
        .expect("bundle must be readable");
        let mut archive = tar::Archive::new(reader);
        let entries = archive.entries().expect("entries must be listed");
        let names = entries
            .map(|entry| {
                entry
                    .expect("entry must be readable")
                    .path()
                    .expect("entry path must be available")
                    .to_string_lossy()
                    .to_string()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "manifest.json".to_owned(),
                object_a.ddl_path,
                object_b.ddl_path,
                object_a.data_path,
                object_b.data_path,
            ]
        );
    }
}
