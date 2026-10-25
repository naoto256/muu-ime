//! Synthetic dictionary setup through the same file and edit transactions
//! used by the host. No whole-snapshot publication bypass is provided.
use super::*;
use crate::imported_dictionary_file::sha256;
use std::collections::BTreeMap;

impl Core {
    pub(crate) fn initialize_fixture_data(
        &self,
        user: &UserDictionarySnapshot,
        learning: &PersonalFrequencySnapshot,
    ) -> Result<(), SessionError> {
        let file = encode_user_dictionary_file(user).expect("synthetic User file");
        self.initialize_data(StoredFileState::Present, &file, learning)
    }

    pub(crate) fn set_user_fixture(
        &self,
        snapshot: UserDictionarySnapshot,
    ) -> Result<(), UserDictionaryEditError> {
        // Validate this complete fixture before editing any existing rows.
        // Malformed-command tests call prepare directly, without this setup.
        let _file =
            encode_user_dictionary_file(&snapshot).map_err(UserDictionaryEditError::File)?;
        snapshot
            .clone()
            .into_overlay()
            .map_err(|_| UserDictionaryEditError::LayerInvalid)?;
        let same = self.vocabulary.user_management().snapshot.as_deref() == Some(&snapshot);
        if same && snapshot.entries.is_empty() {
            return Ok(());
        }
        if !same {
            loop {
                let view = serde_json::to_value(self.user_dictionary_view()).unwrap();
                let Some(entry) = view["entries"].as_array().and_then(|rows| rows.first()) else {
                    break;
                };
                let prepared = self.prepare_user_dictionary(UserDictionaryEditCommand::Remove {
                    expected_revision: self.user_dictionary_view().management_revision,
                    entry_id: entry["entry_id"].as_str().unwrap().into(),
                })?;
                self.publish_user_dictionary(&prepared.ticket)?;
                self.cancel_user_dictionary(&prepared.ticket);
            }
        }
        for row in snapshot.entries {
            // IDs belong to the current management view; an earlier edit can
            // replace them. Never reuse an ID captured before another edit.
            let view = serde_json::to_value(self.user_dictionary_view()).unwrap();
            let entry_id = if same {
                view["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|entry| {
                        entry["reading"] == row.reading && entry["surface"] == row.surface
                    })
                    .map(|entry| entry["entry_id"].as_str().unwrap().into())
            } else {
                None
            };
            let prepared = self.prepare_user_dictionary(UserDictionaryEditCommand::Upsert {
                expected_revision: self.user_dictionary_view().management_revision,
                entry_id,
                reading: row.reading,
                surface: row.surface,
                part_of_speech: row.part_of_speech,
                priority: row.priority,
            })?;
            self.publish_user_dictionary(&prepared.ticket)?;
            self.cancel_user_dictionary(&prepared.ticket);
        }
        Ok(())
    }

    pub(crate) fn set_imported_fixture(
        &self,
        snapshot: ImportedDictionaryCatalogSnapshot,
    ) -> Result<(), ImportedDictionaryEditError> {
        let mut files = BTreeMap::new();
        // Existing enabled fixture documents can be reconstructed exactly:
        // these helpers always encode the canonical native file themselves.
        let current = self.vocabulary.imported_management();
        for summary in current
            .management
            .dictionaries
            .as_ref()
            .into_iter()
            .flatten()
        {
            if let Some(part) = current.parts.get(&summary.manifest.metadata.id) {
                let bytes = encode_imported_dictionary_file(&ImportedDictionaryDocument {
                    metadata: summary.manifest.metadata.clone(),
                    entries: part.records.clone(),
                })?;
                assert_eq!(sha256(&bytes), summary.manifest.blob_sha256);
                files.insert(summary.manifest.blob_sha256.clone(), bytes);
            }
        }
        let mut docs = Vec::new();
        for dictionary in snapshot.dictionaries {
            let document = ImportedDictionaryDocument {
                metadata: ImportedDictionaryMetadata {
                    id: dictionary.dictionary_id,
                    name: "Synthetic dictionary".into(),
                    version: "1".into(),
                    author: "Test fixture".into(),
                    license: "CC0".into(),
                    provenance: "Self-authored".into(),
                },
                entries: dictionary.entries,
            };
            let bytes = encode_imported_dictionary_file(&document)?;
            docs.push((document, dictionary.enabled, bytes));
        }
        if !current.management.initialized {
            let manifest = ImportedDictionaryManifest {
                schema_version: 2,
                dictionaries: docs
                    .iter()
                    .enumerate()
                    .map(|(index, (doc, enabled, bytes))| {
                        let hash = sha256(bytes);
                        files.insert(hash.clone(), bytes.clone());
                        ImportedDictionaryManifestEntry {
                            metadata: doc.metadata.clone(),
                            enabled: *enabled,
                            imported_at_unix_ms: 1,
                            source_sha256: hash.clone(),
                            blob_sha256: hash,
                            order: index as i64,
                        }
                    })
                    .collect(),
            };
            let bytes = encode_imported_dictionary_manifest(&manifest)?;
            let hash = sha256(&bytes);
            files.insert(hash.clone(), bytes);
            let reply = self.begin_imported_dictionary_load(
                ImportedDictionaryLoadCommand {
                    expected_revision: self.imported_dictionary_view().management_revision,
                    file_state: StoredFileState::Present,
                    now_unix_ms: 1,
                },
                format!("{hash}\n").as_bytes(),
            )?;
            return imported_finish(self, reply, &mut files, true);
        }
        // Reconfiguration after startup is an edit, never another load.
        for summary in current
            .management
            .dictionaries
            .as_ref()
            .into_iter()
            .flatten()
        {
            let reply = self.prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Remove {
                    expected_revision: self.imported_dictionary_view().management_revision,
                    dictionary_id: summary.manifest.metadata.id.clone(),
                },
                &[],
            )?;
            imported_finish(self, reply, &mut files, false)?;
        }
        for (_, enabled, bytes) in docs {
            let reply = self.prepare_imported_dictionary(
                ImportedDictionaryEditCommand::Import {
                    expected_revision: self.imported_dictionary_view().management_revision,
                    source_file_name: "fixture.muudict".into(),
                    imported_at_unix_ms: 1,
                    enabled,
                },
                &bytes,
            )?;
            imported_finish(self, reply, &mut files, false)?;
        }
        Ok(())
    }

    pub(crate) fn set_postal_fixture(
        &self,
        snapshot: PostalCodeSnapshot,
    ) -> Result<(), PostalCodeEditError> {
        let tsv = encode_postal_code_tsv(&snapshot)?;
        let manifest = encode_postal_code_manifest(&PostalCodeManifest {
            schema_version: 1,
            source_url: POSTAL_SOURCE_URL.into(),
            source_page_url: POSTAL_SOURCE_PAGE_URL.into(),
            source_last_modified: "unknown".into(),
            retrieved_at: "2026-01-01T00:00:00Z".into(),
            zip_sha256: "0".repeat(64),
            csv_sha256: "1".repeat(64),
            snapshot_sha256: sha256(&tsv),
            entry_count: snapshot.entries.len(),
        })?;
        let generation = sha256(&manifest);
        let mut reply = self.begin_postal_code_load(
            PostalCodeLoadCommand {
                expected_revision: self.postal_code_view().management_revision,
                current_state: StoredFileState::Present,
            },
            format!("{generation}\n").as_bytes(),
        )?;
        for bytes in [&manifest, &tsv] {
            let plan: serde_json::Value = serde_json::from_slice(&reply.plan).unwrap();
            reply = self.supply_postal_code_load(
                PostalCodeSupplyCommand {
                    ticket: reply.ticket,
                    request_id: plan["request"]["request_id"].as_u64().unwrap(),
                    file_state: StoredFileState::Present,
                },
                bytes,
            )?;
        }
        self.publish_postal_code(&reply.ticket)?;
        let view = self.postal_code_view();
        self.finish_postal_code(PostalCodeFinishCommand {
            ticket: reply.ticket.clone(),
            outcome: PostalCodeFinishKind::Loaded,
            revision: view.management_revision,
            generation: view.generation,
        })?;
        self.cancel_postal_code(&reply.ticket)
    }
}

fn imported_finish(
    core: &Core,
    mut reply: PreparedImportedDictionaryReply,
    files: &mut BTreeMap<String, Vec<u8>>,
    load: bool,
) -> Result<(), ImportedDictionaryEditError> {
    for _ in 0..100 {
        let step: serde_json::Value = serde_json::from_slice(&reply.plan_json).unwrap();
        if step["state"] == "ready" {
            let plan = &step["ready"];
            core.publish_imported_dictionary(&reply.ticket)?;
            core.finish_imported_dictionary(ImportedDictionaryFinishCommand {
                ticket: reply.ticket.clone(),
                outcome: if load {
                    ImportedDictionaryFinishKind::Loaded
                } else {
                    ImportedDictionaryFinishKind::NewPointerDurable
                },
                revision: plan["published_revision"].as_u64().unwrap(),
                generation_sha256: plan["generation_sha256"].as_str().map(str::to_owned),
            })?;
            return core.cancel_imported_dictionary(&reply.ticket);
        }
        let (command, bytes) = if let Some(hash) = step["write_blob"]["sha256"].as_str() {
            assert!(!load);
            let bytes = core.imported_dictionary_prepared_blob(&reply.ticket, hash)?;
            files.insert(hash.into(), bytes);
            (
                ImportedDictionarySupplyCommand::Written {
                    ticket: reply.ticket,
                    sha256: hash.into(),
                },
                Vec::new(),
            )
        } else {
            let request = &step["request"];
            let bytes = request["sha256"]
                .as_str()
                .and_then(|hash| files.get(hash))
                .cloned();
            (
                ImportedDictionarySupplyCommand::Read {
                    ticket: reply.ticket,
                    request_id: request["request_id"].as_u64().unwrap(),
                    file_state: if bytes.is_some() {
                        StoredFileState::Present
                    } else {
                        StoredFileState::Missing
                    },
                },
                bytes.unwrap_or_default(),
            )
        };
        reply = if load {
            core.supply_imported_dictionary_load(command, &bytes)?
        } else {
            core.supply_imported_dictionary_prepare(command, &bytes)?
        };
    }
    panic!("finite imported fixture did not finish")
}
