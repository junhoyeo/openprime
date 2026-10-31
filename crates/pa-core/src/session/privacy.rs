//! Bounded whole-container privacy validation, before repair or cache shortcuts.
//! Takes an already-open file so the caller retains its ownership/identity guard.
//! This is a structural data check, not an authorization to mutate the path.
use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

use pa_types::session::privacy::{decode_privacy_row, AdmissionPrivacy};
use sha2::{Digest, Sha256};

/// Bounds are refusal thresholds, never permission to accept a partial scan.
#[derive(Debug, Clone, Copy)]
pub struct PrivacyScanLimits {
    pub max_bytes: u64,
    pub max_row_bytes: usize,
    pub max_rows: usize,
}
impl Default for PrivacyScanLimits {
    fn default() -> Self {
        Self { max_bytes:256*1024*1024, max_row_bytes:8*1024*1024, max_rows:1_000_000 }
    }
}

/// Validated full-container result. Absent carrier never certifies historical consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPrivacyScan {
    pub session_id: String,
    pub restriction: Option<AdmissionPrivacy>,
    pub rows: usize,
    pub bytes: u64,
    pub digest: [u8;32],
}

fn invalid(message: &'static str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData,message) }

/// Strictly scan the complete opened container and restore the caller's position.
/// No message/row bytes are included in diagnostics. A complete final JSON object
/// may omit its newline; a truncated object or trailing garbage still refuses.
///
/// # Errors
/// Returns an error for malformed/duplicate/unsupported policy, incomplete input,
/// I/O, bounds or an observed change to the opened file while scanning.
pub fn scan_opened_container(file: &mut File, limits: PrivacyScanLimits) -> io::Result<SessionPrivacyScan> {
    let position=file.stream_position()?;
    let result=scan(file,limits);
    let restored=file.seek(SeekFrom::Start(position));
    match (result,restored) {
        (Ok(result),Ok(_))=>Ok(result),
        (Err(error),_)|(_,Err(error))=>Err(error),
    }
}

fn scan(file: &mut File, limits: PrivacyScanLimits) -> io::Result<SessionPrivacyScan> {
    let before=file.metadata()?;
    if !before.is_file() { return Err(invalid("session privacy input is not a regular file")); }
    if before.len()>limits.max_bytes { return Err(invalid("session privacy scan exceeds byte limit")); }
    file.seek(SeekFrom::Start(0))?;
    let mut reader=BufReader::new(&mut *file);
    let mut line=Vec::new();
    let mut rows=0;
    let mut bytes=0_u64;
    let mut session_id=None;
    let mut restriction=None;
    let mut ids=HashSet::new();
    let mut hash=Sha256::new();
    loop {
        line.clear();
        let read=(&mut reader).take(limits.max_row_bytes.saturating_add(1) as u64)
            .read_until(b'\n',&mut line)?;
        if read==0 { break; }
        bytes=bytes.checked_add(read as u64).ok_or_else(||invalid("session privacy scan byte overflow"))?;
        if read>limits.max_row_bytes || bytes>limits.max_bytes || rows>=limits.max_rows {
            return Err(invalid("session privacy scan exceeds configured limit"));
        }
        hash.update(&line);
        if line.iter().all(u8::is_ascii_whitespace) { continue; }
        let (value,policy)=decode_privacy_row(&line).map_err(|_|invalid("session privacy input has malformed or conflicting metadata"))?;
        if rows==0 {
            if value.get("type").and_then(serde_json::Value::as_str)!=Some("session") {
                return Err(invalid("session privacy input is missing its header"));
            }
            let id=value.get("id").and_then(serde_json::Value::as_str)
                .filter(|id|!id.is_empty()&&!id.contains('\0'))
                .ok_or_else(||invalid("session privacy header identity is invalid"))?;
            session_id=Some(id.to_string());
        } else if value.get("type").and_then(serde_json::Value::as_str)==Some("session") {
            return Err(invalid("session privacy input has multiple headers"));
        }
        if let Some(id)=value.get("id").and_then(serde_json::Value::as_str) {
            if !ids.insert(id.to_string()) { return Err(invalid("session privacy input has duplicate entry identities")); }
        }
        if policy.is_some() { restriction=policy; }
        rows+=1;
    }
    drop(reader);
    let after=file.metadata()?;
    if before.len()!=after.len() || before.modified()?!=after.modified()? || bytes!=after.len() {
        return Err(invalid("session privacy input changed during validation"));
    }
    let session_id=session_id.ok_or_else(||invalid("session privacy input is empty"))?;
    Ok(SessionPrivacyScan { session_id,restriction,rows,bytes,digest:hash.finalize().into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session::{EntryBase,FileEntry};
    use serde_json::{json,Map};
    fn rows() -> Vec<serde_json::Value> {
        let marker=AdmissionPrivacy::RestrictedV1.entry(EntryBase {
            id:Some("off-branch-marker".into()),parent_id:Some("old-leaf".into()),
            timestamp:Some("2026-10-30T00:00:00.000Z".into()),rest:Map::default(),
        }).unwrap();
        assert!(matches!(marker,FileEntry::Custom{..}));
        vec![json!({"type":"session","version":3,"id":"synthetic-session"}),
            json!({"type":"message","id":"old-leaf","parentId":null}),
            serde_json::to_value(marker).unwrap(),
            json!({"type":"message","id":"selected-leaf","parentId":null})]
    }
    #[test]
    fn actual_serialized_off_branch_carrier_is_folded_from_entire_file() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("session.jsonl");
        let content=rows().iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")+"\n";
        std::fs::write(&path,&content).unwrap();
        let mut file=File::open(&path).unwrap();file.seek(SeekFrom::Start(7)).unwrap();
        let result=scan_opened_container(&mut file,PrivacyScanLimits::default()).unwrap();
        assert_eq!((result.session_id.as_str(),result.restriction,result.rows),
            ("synthetic-session",Some(AdmissionPrivacy::RestrictedV1),4));
        assert_eq!(file.stream_position().unwrap(),7);
        assert_eq!(std::fs::read_to_string(path).unwrap(),content);
    }
    #[test]
    fn incomplete_or_capped_input_is_refused_without_repair() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("session.jsonl");
        let content=rows().iter().map(ToString::to_string).collect::<Vec<_>>().join("\n")+"\n{\"customType\":";
        std::fs::write(&path,&content).unwrap();
        let mut file=File::open(&path).unwrap();
        assert!(scan_opened_container(&mut file,PrivacyScanLimits::default()).is_err());
        assert!(scan_opened_container(&mut file,PrivacyScanLimits{max_rows:1,..PrivacyScanLimits::default()}).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(),content);
    }
    #[test]
    fn complete_final_object_needs_no_newline_but_partial_or_trailing_bytes_refuse() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("no-newline.jsonl");
        let public=serde_json::json!({"type":"session","version":3,"id":"public"}).to_string();
        std::fs::write(&path,&public).unwrap();
        let mut file=File::open(&path).unwrap();
        let scan=scan_opened_container(&mut file,PrivacyScanLimits::default()).unwrap();
        assert_eq!((scan.session_id.as_str(),scan.restriction),("public",None));
        for suffix in [" trailing", "\n{\"customType\":", "\n{\"type\":\"custom\",\"customType\":\"admission_privacy_state\""] {
            let invalid=public.clone()+suffix;std::fs::write(&path,&invalid).unwrap();
            let mut file=File::open(&path).unwrap();
            assert!(scan_opened_container(&mut file,PrivacyScanLimits::default()).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(),invalid);
        }
    }

}
