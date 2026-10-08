//! Proton Drive ("My files"): browse with decrypted names and download,
//! straight from Proton's API. Names and contents are decrypted here, on this
//! computer; Proton only ever sees ciphertext.

use crate::client::{Client, array, b64};
use crate::error::{Error, Result};
use crate::pgp::Key;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};

const BATCH: usize = 100;

#[derive(Clone, Debug)]
pub struct Node {
    pub id: String,
    pub volume: String,
    pub parent: Option<String>,
    pub name: String,
    pub folder: bool,
    /// Plain size in bytes, when known.
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: i64,
    pub mime: String,
    content_key_packet: Option<String>,
    revision: Option<String>,
}

impl Node {
    /// A bare node for other crates' tests.
    #[doc(hidden)]
    pub fn for_tests(id: &str, parent: Option<&str>, name: &str) -> Node {
        Node {
            id: id.to_owned(),
            volume: "v".into(),
            parent: parent.map(str::to_owned),
            name: name.to_owned(),
            folder: true,
            size: None,
            modified: 0,
            mime: String::new(),
            content_key_packet: None,
            revision: None,
        }
    }
}

impl Client {
    /// The root of "My files".
    pub fn drive_root(&self) -> Result<Node> {
        let mine = self.api.get("drive/v2/shares/my-files", &[])?;
        let text = |p: &str| mine.pointer(p).and_then(Value::as_str).map(str::to_owned);
        let need = |p: &str| text(p).ok_or_else(|| Error::Protocol(format!("my-files misses {p}")));
        let (volume, root_id) = (need("/Volume/VolumeID")?, need("/Link/Link/LinkID")?);
        let address = need("/Share/AddressID")?;

        let ring = self.keyring()?;
        let address_keys = ring.address(&address).ok_or_else(|| Error::Crypto("share address key missing".into()))?;
        let passphrase = self.pgp.decrypt_armored(&need("/Share/Passphrase")?, &address_keys.keys)?;
        let share_key = self.pgp.unlock(&need("/Share/Key")?, &passphrase)?;

        let root = self.drive_links(&volume, std::slice::from_ref(&root_id), &share_key)?;
        let mut root = root.into_iter().next().ok_or_else(|| Error::Protocol("root folder not returned".into()))?;
        root.name = "My files".into();
        root.parent = None;
        Ok(root)
    }

    /// The contents of a folder, folders first, then by name.
    pub fn drive_children(&self, folder: &Node) -> Result<Vec<Node>> {
        if !folder.folder {
            return Err(Error::Unsupported("That isn't a folder".into()));
        }
        let parent_key = self
            .node_keys
            .lock()
            .unwrap()
            .get(&folder.id)
            .cloned()
            .ok_or_else(|| Error::Crypto("folder key isn't loaded".into()))?;
        let mut ids = Vec::new();
        let mut anchor = String::new();
        loop {
            let mut query = Vec::new();
            if !anchor.is_empty() {
                query.push(("AnchorID", anchor.clone()));
            }
            let page = self.api.get(&format!("drive/v2/volumes/{}/folders/{}/children", folder.volume, folder.id), &query)?;
            ids.extend(array(&page, "/LinkIDs").into_iter().filter_map(Value::as_str).map(str::to_owned));
            match (page.get("More").and_then(Value::as_bool), page.get("AnchorID").and_then(Value::as_str)) {
                (Some(true), Some(next)) if next != anchor => anchor = next.to_owned(),
                _ => break,
            }
        }
        let mut nodes = Vec::new();
        for chunk in ids.chunks(BATCH) {
            nodes.extend(self.drive_links(&folder.volume, chunk, &parent_key)?);
        }
        nodes.sort_by(|a, b| b.folder.cmp(&a.folder).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        Ok(nodes)
    }

    /// Visit every folder below `root` breadth-first until `limit` items have
    /// been seen. `visit` gets each folder's children as they arrive and
    /// returns false to stop.
    pub fn drive_walk(&self, root: &Node, limit: usize, visit: &mut dyn FnMut(&[Node]) -> bool) -> Result<()> {
        let mut queue = VecDeque::from([root.clone()]);
        let mut seen = 0;
        while let Some(folder) = queue.pop_front() {
            let children = self.drive_children(&folder)?;
            seen += children.len();
            queue.extend(children.iter().filter(|n| n.folder).cloned());
            if !visit(&children) || seen >= limit {
                break;
            }
        }
        Ok(())
    }

    /// Decrypt the metadata of `ids` (all children of the node holding `parent_key`).
    fn drive_links(&self, volume: &str, ids: &[String], parent_key: &Key) -> Result<Vec<Node>> {
        let answer = self.api.post(&format!("drive/v2/volumes/{volume}/links"), &json!({ "LinkIDs": ids }))?;
        let mut out = Vec::new();
        for entry in array(&answer, "/Links") {
            let Some(link) = entry.get("Link") else { continue };
            let text = |key: &str| link.get(key).and_then(Value::as_str).map(str::to_owned);
            let (Some(id), Some(kind)) = (text("LinkID"), link.get("Type").and_then(Value::as_u64)) else { continue };
            let folder = match kind {
                1 => true,
                2 => false,
                _ => continue,
            };
            let file = entry.get("File");
            // Drafts have no committed revision yet.
            let revision = file
                .and_then(|f| f.pointer("/ActiveRevision/RevisionID"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if !folder && revision.is_none() {
                continue;
            }
            let node_key = text("NodePassphrase")
                .zip(text("NodeKey"))
                .ok_or_else(|| Error::Protocol("link without keys".into()))
                .and_then(|(passphrase, key)| {
                    let passphrase = self.pgp.decrypt_armored(&passphrase, std::slice::from_ref(parent_key))?;
                    self.pgp.unlock(&key, &passphrase)
                });
            let Ok(node_key) = node_key else {
                out.push(unreadable(volume, &id, text("ParentLinkID"), link));
                continue;
            };
            let name = text("Name")
                .and_then(|n| self.pgp.decrypt_armored(&n, std::slice::from_ref(parent_key)).ok())
                .and_then(|n| String::from_utf8(n).ok())
                .unwrap_or_else(|| "(unreadable name)".to_owned());

            let xattr = if folder { entry.pointer("/Folder/XAttr") } else { file.and_then(|f| f.pointer("/ActiveRevision/XAttr")) }
                .and_then(Value::as_str)
                .and_then(|x| self.pgp.decrypt_armored(x, std::slice::from_ref(&node_key)).ok())
                .and_then(|x| serde_json::from_slice::<Value>(&x).ok());
            let size = xattr
                .as_ref()
                .and_then(|x| x.pointer("/Common/Size").and_then(Value::as_u64))
                .or_else(|| file.and_then(|f| f.get("TotalEncryptedSize")).and_then(Value::as_u64));

            self.node_keys.lock().unwrap().insert(id.clone(), node_key);
            out.push(Node {
                id,
                volume: volume.to_owned(),
                parent: text("ParentLinkID"),
                name,
                folder,
                size: if folder { None } else { size },
                modified: link.get("ModifyTime").and_then(Value::as_i64).unwrap_or(0),
                mime: file.and_then(|f| f.get("MediaType")).and_then(Value::as_str).unwrap_or_default().to_owned(),
                content_key_packet: file.and_then(|f| f.get("ContentKeyPacket")).and_then(Value::as_str).map(str::to_owned),
                revision,
            });
        }
        Ok(out)
    }

    /// Download and decrypt a file into `dir`, returning where it landed.
    /// `progress` gets (blocks done, blocks total so far).
    pub fn drive_download(&self, file: &Node, dir: &Path, progress: &mut dyn FnMut(usize)) -> Result<PathBuf> {
        let (Some(packet), Some(revision)) = (&file.content_key_packet, &file.revision) else {
            return Err(Error::Unsupported("That isn't a file".into()));
        };
        let node_key = self
            .node_keys
            .lock()
            .unwrap()
            .get(&file.id)
            .cloned()
            .ok_or_else(|| Error::Crypto("file key isn't loaded".into()))?;
        let session = self.pgp.session_key(&b64(packet)?, std::slice::from_ref(&node_key))?;

        std::fs::create_dir_all(dir).map_err(io)?;
        let (target, mut out) = create_unique(dir, &file.name)?;
        let result = (|| -> Result<()> {
            let mut from = 1usize;
            let mut done = 0usize;
            loop {
                let page = self.api.get(
                    &format!("drive/v2/volumes/{}/files/{}/revisions/{}", file.volume, file.id, revision),
                    &[("PageSize", "50".into()), ("FromBlockIndex", from.to_string())],
                )?;
                let blocks = array(&page, "/Revision/Blocks");
                if blocks.is_empty() {
                    return Ok(());
                }
                for block in blocks {
                    let text = |k: &str| block.get(k).and_then(Value::as_str).unwrap_or_default();
                    let encrypted = self.api.download(text("BareURL"), text("Token"))?;
                    if STANDARD.encode(Sha256::digest(&encrypted)) != text("Hash") {
                        return Err(Error::Crypto("a downloaded block failed its integrity check".into()));
                    }
                    out.write_all(&self.pgp.decrypt_with(&encrypted, &session)?).map_err(io)?;
                    from = block.get("Index").and_then(Value::as_u64).unwrap_or(from as u64) as usize + 1;
                    done += 1;
                    progress(done);
                }
            }
        })();
        match result.and_then(|_| out.sync_all().map_err(io)) {
            Ok(()) => Ok(target),
            Err(error) => {
                let _ = std::fs::remove_file(&target);
                Err(error)
            }
        }
    }
}

fn unreadable(volume: &str, id: &str, parent: Option<String>, link: &Value) -> Node {
    Node {
        id: id.to_owned(),
        volume: volume.to_owned(),
        parent,
        name: "(can't be decrypted)".into(),
        folder: false,
        size: None,
        modified: link.get("ModifyTime").and_then(Value::as_i64).unwrap_or(0),
        mime: String::new(),
        content_key_packet: None,
        revision: None,
    }
}

fn io(error: std::io::Error) -> Error {
    Error::Unsupported(format!("Couldn't write the file: {error}"))
}

/// A file name that can't escape the folder or be hidden.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' || c.is_control() { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cleaned: String = cleaned.chars().take(200).collect();
    if cleaned.is_empty() { "Proton Drive file".to_owned() } else { cleaned }
}

/// Create `dir/name`, or `name (2)`… when it exists. Never overwrites.
fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, std::fs::File)> {
    let name = safe_file_name(name);
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name.as_str(), ""),
    };
    for n in 1..1000 {
        let candidate = if n == 1 { name.clone() } else { format!("{stem} ({n}){ext}") };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io(e)),
        }
    }
    Err(Error::Unsupported("Too many files with that name".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_or_hide() {
        assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("   "), "Proton Drive file");
        assert_eq!(safe_file_name("a\0b"), "a_b");
        assert_eq!(safe_file_name("report.pdf"), "report.pdf");
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("spotty-drive-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, _) = create_unique(&dir, "x.txt").unwrap();
        let (b, _) = create_unique(&dir, "x.txt").unwrap();
        assert_eq!(a.file_name().unwrap(), "x.txt");
        assert_eq!(b.file_name().unwrap(), "x (2).txt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
