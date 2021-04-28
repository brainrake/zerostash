#![allow(unused)]

use crate::{
    backends::Backend,
    chunks::ChunkPointer,
    compress,
    crypto::CryptoProvider,
    files::{self, FileIndex},
    object::*,
};

use flume as mpsc;
use itertools::Itertools;
use memmap2::MmapOptions;
use tokio::{fs, task};

use std::{
    collections::HashMap,
    env,
    error::Error,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::UNIX_EPOCH,
};

type ThreadWork = (PathBuf, Arc<files::Entry>);

type Sender = mpsc::Sender<ThreadWork>;
type Receiver = mpsc::Receiver<ThreadWork>;

pub type FileIterator<'a> = Box<(dyn Iterator<Item = Arc<files::Entry>> + 'a)>;

pub async fn from_iter(
    max_file_handles: usize,
    iter: FileIterator<'_>,
    backend: Arc<dyn Backend>,
    crypto: impl CryptoProvider + 'static,
    target: impl AsRef<Path>,
) {
    let (mut sender, receiver) = mpsc::bounded(max_file_handles);

    // TODO this is single-threaded
    task::spawn(process_packet_loop(receiver, backend, crypto));

    for md in iter {
        let path = get_path(&md.name);

        // if there's no parent, then the entire thing is root.
        // if what we're trying to extract is root, then what happens?
        let mut basedir = target.as_ref().to_owned();
        if let Some(parent) = path.parent() {
            // create the file and parent directory
            fs::create_dir_all(basedir.join(parent)).await.unwrap();
        }

        let filename = basedir.join(&path);

        if sender.send_async((filename, md.clone())).await.is_err() {
            println!("internal process crashed");
            return;
        }
    }
}

async fn process_packet_loop(
    mut r: Receiver,
    backend: Arc<dyn Backend>,
    crypto: impl CryptoProvider,
) {
    // Since resources here are all managed by RAII, and they all
    // implement Drop, we can simply go through the Arc<_>s,
    // mmap them, open the corresponding objects to extract details,
    // and everything will be cleaned up on Drop.
    //
    // In fact, every layer of these for loops is also managing a
    // corresponding resource.
    let mut buffer = WriteObject::default();

    // This loop is managing an mmap of a file that's written
    while let Ok((filename, metadata)) = r.recv_async().await {
        if metadata.size == 0 {
            continue;
        }
        let fd = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(filename)
            .await
            .unwrap();
        fd.set_len(metadata.size).await.unwrap();

        let object_ordered = metadata.chunks.iter().fold(HashMap::new(), |mut a, c| {
            a.entry(c.1.file).or_insert_with(Vec::new).push(c);
            a
        });

        let mut mmap = unsafe {
            MmapOptions::new()
                .len(metadata.size as usize)
                .map_mut(&fd.into_std().await)
                .expect("mmap")
        };

        // This loop manages the object we're reading from
        for (objectid, cs) in object_ordered.iter() {
            let object = backend.read_object(objectid).await.expect("object read");

            // This loop will extract & decrypt & decompress from the object
            for (i, (start, cp)) in cs.iter().enumerate() {
                let start = *start as usize;
                let mut target: &mut [u8] = buffer.as_inner_mut();

                let len = crypto.decrypt_chunk(&mut target, &object, cp);
                compress::decompress_into(&mut mmap[start..], &target[..len]).unwrap();
            }
        }
    }
}

fn get_path(filename: impl AsRef<Path>) -> PathBuf {
    let path = filename.as_ref();
    let mut cs = path.components();

    if let Some(std::path::Component::RootDir) = cs.next() {
        cs.as_path().to_owned()
    } else {
        path.to_owned()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn path_removes_root() {
        use super::*;

        assert_eq!(Path::new("home/a/b"), get_path("/home/a/b").as_path());
        assert_eq!(Path::new("./a/b"), get_path("./a/b").as_path());
    }
}
