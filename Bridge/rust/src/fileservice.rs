//! CoreDevice file service client (com.apple.coredevice.fileservice.control).
//! Protocol per pymobiledevice3 `remote/core_device/file_service.py`.
//!
//! A session is scoped to one domain; we use AppDataContainer (domain 1) with
//! the target app's bundle ID, giving container-relative POSIX I/O.

use idevice::xpc::{Dictionary, XPCObject};
use idevice::{ReadWrite, RemoteXpcClient};

use crate::tunnel::AppDeviceTunnel;

const FILE_CHUNK: usize = 512 * 1024;

// Darwin open(2) flags
const O_WRONLY: i64 = 0x1;
const O_CREAT: i64 = 0x200;
const O_TRUNC: i64 = 0x400;

pub struct FsClient {
    inner: RemoteXpcClient<Box<dyn ReadWrite>>,
    session: String,
}

fn msg(pairs: Vec<(&str, XPCObject)>) -> XPCObject {
    let mut d = Dictionary::new();
    for (k, v) in pairs {
        d.insert(k.to_string(), v);
    }
    XPCObject::Dictionary(d)
}

impl FsClient {
    pub async fn connect(
        tunnel: &mut AppDeviceTunnel,
        bundle_id: &str,
    ) -> Result<FsClient, String> {
        let stream = tunnel
            .connect_xpc_service("com.apple.coredevice.fileservice.control")
            .await?;
        let mut inner = RemoteXpcClient::new(stream)
            .await
            .map_err(|e| format!("file service XPC init: {e:?}"))?;
        inner
            .do_handshake()
            .await
            .map_err(|e| format!("file service XPC handshake: {e:?}"))?;

        let res = Self::send_receive_static(
            &mut inner,
            msg(vec![
                ("Cmd", XPCObject::String("CreateSession".into())),
                ("Domain", XPCObject::UInt64(1)),
                ("Identifier", XPCObject::String(bundle_id.into())),
                ("Session", XPCObject::String(String::new())),
                ("User", XPCObject::String("mobile".into())),
            ]),
        )
        .await?;

        let session = res
            .as_dictionary()
            .and_then(|d| d.get("NewSessionID"))
            .and_then(|v| v.as_string())
            .ok_or_else(|| format!("file service: missing NewSessionID in {res:?}"))?
            .to_string();

        Ok(FsClient { inner, session })
    }

    async fn send_receive(&mut self, request: XPCObject) -> Result<plist::Value, String> {
        Self::send_receive_static(&mut self.inner, request).await
    }

    async fn send_receive_static(
        inner: &mut RemoteXpcClient<Box<dyn ReadWrite>>,
        request: XPCObject,
    ) -> Result<plist::Value, String> {
        inner
            .send_object(request, true)
            .await
            .map_err(|e| format!("file service send: {e:?}"))?;
        // CreateSession replies on the reply channel, later commands on the
        // root channel — wait on both.
        let res = inner
            .recv_any()
            .await
            .map_err(|e| format!("file service recv: {e:?}"))?;

        if let Some(dict) = res.as_dictionary() {
            if dict.contains_key("EncodedError") {
                let detail = dict
                    .get("LocalizedDescription")
                    .and_then(|v| v.as_string())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{:?}", dict.get("EncodedError")));
                return Err(format!("file service: {detail}"));
            }
        }
        Ok(res)
    }

    async fn fs_operation(
        &mut self,
        op: &str,
        args: Vec<(&str, XPCObject)>,
    ) -> Result<plist::Value, String> {
        let mut pairs = vec![
            ("Cmd", XPCObject::String("FileSystemOperation".into())),
            ("OperationType", XPCObject::String(op.into())),
            ("SessionID", XPCObject::String(self.session.clone())),
        ];
        pairs.extend(args);
        self.send_receive(msg(pairs)).await
    }

    /// Names of entries in `path` (container-relative, "." or "" = root).
    pub async fn list(&mut self, path: &str) -> Result<Vec<String>, String> {
        let res = self
            .send_receive(msg(vec![
                ("Cmd", XPCObject::String("RetrieveDirectoryList".into())),
                (
                    "MessageUUID",
                    XPCObject::String(uuid::Uuid::new_v4().to_string()),
                ),
                ("Path", XPCObject::String(path.into())),
                ("SessionID", XPCObject::String(self.session.clone())),
            ]))
            .await?;

        let list = res
            .as_dictionary()
            .and_then(|d| d.get("FileList"))
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("file service: missing FileList in {res:?}"))?;
        Ok(list
            .iter()
            .filter_map(|x| x.as_string().map(str::to_string))
            .collect())
    }

    /// Attributes as a parsed JSON object (fileType, size, permissions, ...).
    pub async fn attributes(&mut self, path: &str) -> Result<serde_json::Value, String> {
        let res = self
            .fs_operation(
                "GetAttributes",
                vec![
                    ("Path", XPCObject::String(path.into())),
                    ("FollowSymlinks", XPCObject::Bool(true)),
                ],
            )
            .await?;
        let raw = res
            .as_dictionary()
            .and_then(|d| d.get("Attributes"))
            .and_then(|v| v.as_string())
            .ok_or_else(|| format!("file service: missing Attributes in {res:?}"))?;
        serde_json::from_str(raw).map_err(|e| format!("attributes JSON: {e}"))
    }

    pub async fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let fd = self.open(path, 0).await?;
        let mut out = Vec::new();
        let res = async {
            loop {
                let chunk = self
                    .fs_operation(
                        "ReadFile",
                        vec![
                            ("FileDescriptor", XPCObject::Int64(fd)),
                            ("Offset", XPCObject::UInt64(out.len() as u64)),
                            ("Length", XPCObject::Int64(FILE_CHUNK as i64)),
                        ],
                    )
                    .await?;
                let data = chunk
                    .as_dictionary()
                    .and_then(|d| d.get("FileData"))
                    .and_then(|v| v.as_data())
                    .ok_or_else(|| format!("file service: missing FileData in {chunk:?}"))?;
                if data.is_empty() {
                    break;
                }
                out.extend_from_slice(data);
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = self.close(fd).await;
        res.map(|_| out)
    }

    pub async fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), String> {
        let fd = self.open(path, O_WRONLY | O_CREAT | O_TRUNC).await?;
        let res = async {
            let mut off = 0usize;
            while off < data.len() {
                let end = (off + FILE_CHUNK).min(data.len());
                self.fs_operation(
                    "WriteFile",
                    vec![
                        ("FileDescriptor", XPCObject::Int64(fd)),
                        ("Offset", XPCObject::UInt64(off as u64)),
                        ("FileData", XPCObject::Data(data[off..end].to_vec())),
                    ],
                )
                .await?;
                off = end;
            }
            Ok::<(), String>(())
        }
        .await;
        let _ = self.close(fd).await;
        res
    }

    pub async fn mkdir(&mut self, path: &str) -> Result<(), String> {
        self.fs_operation("CreateDirectory", vec![("Path", XPCObject::String(path.into()))])
            .await
            .map(|_| ())
    }

    pub async fn remove(&mut self, path: &str) -> Result<(), String> {
        let is_dir = self
            .attributes(path)
            .await
            .ok()
            .and_then(|a| a.get("fileType").and_then(|t| t.as_str()).map(str::to_string))
            .map(|t| t == "directory")
            .unwrap_or(false);
        let op = if is_dir { "RemoveDirectory" } else { "RemoveFile" };
        self.fs_operation(op, vec![("Path", XPCObject::String(path.into()))])
            .await
            .map(|_| ())
    }

    pub async fn rename(&mut self, old_path: &str, new_path: &str) -> Result<(), String> {
        self.fs_operation(
            "Rename",
            vec![
                ("OldPath", XPCObject::String(old_path.into())),
                ("NewPath", XPCObject::String(new_path.into())),
            ],
        )
        .await
        .map(|_| ())
    }

    async fn open(&mut self, path: &str, flags: i64) -> Result<i64, String> {
        let res = self
            .fs_operation(
                "OpenFile",
                vec![
                    ("Path", XPCObject::String(path.into())),
                    ("Flags", XPCObject::Int64(flags)),
                    ("Mode", XPCObject::UInt64(0o644)),
                ],
            )
            .await?;
        res.as_dictionary()
            .and_then(|d| d.get("FileDescriptor"))
            .and_then(|v| v.as_signed_integer().or_else(|| v.as_unsigned_integer().map(|u| u as i64)))
            .ok_or_else(|| format!("file service: missing FileDescriptor in {res:?}"))
    }

    async fn close(&mut self, fd: i64) -> Result<(), String> {
        self.fs_operation("CloseFile", vec![("FileDescriptor", XPCObject::Int64(fd))])
            .await
            .map(|_| ())
    }
}
