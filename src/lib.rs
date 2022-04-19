use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, Write};
//use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
};

mod pt_lsm;
use pt_lsm::Lsm;

// live percentage of a file before it's considered rewritabe
const HEAP_DIR_SUFFIX: &str = "heap";
const PT_DIR_SUFFIX: &str = "page_index";
const PT_LSN_KEY: [u8; 8] = u64::MAX.to_le_bytes();

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct PageId(u64);

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct DiskLocation(u64);

const LOCATION_SZ: usize = std::mem::size_of::<DiskLocation>();

struct FileAndMetadata {
    file: File,
    shard: u8,
    location: DiskLocation,
    path: PathBuf,
    capacity: u64,
    len: AtomicU64,
    size_class: u8,
    generation: u8,
}

#[derive(Debug)]
pub struct Config {
    pub path: PathBuf,
    pub target_file_size: u64,
    pub file_compaction_percent: u8,
    /// A partitioning function for pages based on monotonic
    /// page id, page size, and page rewrite generation.
    pub shard_function: fn(PageId, usize, u8) -> u8,
}

pub fn default_shard_function(_pid: PageId, _size: usize, _generation: u8) -> u8 {
    0
}

impl Default for Config {
    fn default() -> Config {
        Config {
            path: "".into(),
            target_file_size: 1 << 28, // 256mb
            file_compaction_percent: 60,
            shard_function: default_shard_function,
        }
    }
}

pub struct Marble {
    // maps from PageId to DiskLocation
    pt: RwLock<Lsm<8, 8>>,
    files: RwLock<BTreeMap<DiskLocation, FileAndMetadata>>,
    next_file_lsn: u64,
    config: Config,
}

impl Marble {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Marble> {
        let config = Config {
            path: path.as_ref().into(),
            ..Config::default()
        };

        Marble::open_with_config(config)
    }

    pub fn open_with_config(config: Config) -> io::Result<Marble> {
        let heap_dir = config.path.join(HEAP_DIR_SUFFIX);

        // initialize directories if not present
        if let Err(e) = fs::read_dir(&heap_dir) {
            if e.kind() == io::ErrorKind::NotFound {
                let _ = fs::create_dir_all(&heap_dir);
            }
        }

        // recover page location index
        let pt = Lsm::<8, 8>::recover(config.path.join(PT_DIR_SUFFIX))?;
        let recovered_pt_lsn = if let Some(max) = pt.get(&PT_LSN_KEY) {
            u64::from_le_bytes(*max)
        } else {
            0
        };

        // parse file names
        // calculate file tenancy

        let mut files = BTreeMap::new();
        let mut max_file_lsn = 0;
        let mut max_file_size = 0;

        for entry_res in fs::read_dir(heap_dir)? {
            let entry = entry_res?;
            let path = entry.path();
            let name = path
                .file_name()
                .expect("file without name encountered in internal directory")
                .to_str()
                .expect("non-utf8 file name encountered in internal directory");

            // remove files w/ temp name
            if name.ends_with("tmp") {
                eprintln!(
                    "removing heap file that was not fully written before the last crash: {:?}",
                    entry.path()
                );

                fs::remove_file(entry.path())?;
                continue;
            }

            let splits: Vec<&str> = name.split("-").collect();
            if splits.len() != 5 {
                eprintln!(
                    "encountered strange file in internal directory: {:?}",
                    entry.path()
                );
                continue;
            }

            let shard: u8 = splits[0]
                .parse()
                .expect("encountered garbage filename in internal directory");
            let lsn: u64 = splits[1]
                .parse()
                .expect("encountered garbage filename in internal directory");
            let size_class: u8 = splits[2]
                .parse()
                .expect("encountered garbage filename in internal directory");
            let generation: u8 = splits[3]
                .parse()
                .expect("encountered garbage filename in internal directory");
            let capacity: u64 = splits[4]
                .parse()
                .expect("encountered garbage filename in internal directory");

            // remove files that are ahead of the recovered page location index
            if lsn > recovered_pt_lsn {
                eprintln!(
                    "removing heap file that has an lsn of {}, \
                    which is higher than the recovered page table lsn of {}",
                    lsn, recovered_pt_lsn,
                );
                fs::remove_file(entry.path())?;
                continue;
            }

            let mut options = OpenOptions::new();
            options.read(true);

            let file = options.open(entry.path())?;
            let location = DiskLocation(lsn);

            let file_size = entry.metadata()?.len();
            max_file_size = max_file_size.max(file_size);
            max_file_lsn = max_file_lsn.max(lsn);

            let file_and_metadata = FileAndMetadata {
                len: 0.into(),
                capacity,
                path: entry.path().into(),
                file,
                location,
                size_class,
                generation,
                shard,
            };

            files.insert(location, file_and_metadata);
        }

        let next_file_lsn = max_file_lsn + max_file_size + 1;

        Ok(Marble {
            pt: RwLock::new(pt),
            files: RwLock::new(files),
            next_file_lsn,
            config,
        })
    }

    pub fn read(&self, pid: PageId) -> io::Result<Box<[u8]>> {
        let mut files = self.files.write().unwrap();
        let shard = 0; // todo!();
        let size_class = 0; // todo!();
        let generation = 0; // todo!();

        let pt = self.pt.read().unwrap();
        let lsn_bytes = *pt.get(&pid.0.to_be_bytes()).unwrap();
        drop(pt);

        let lsn = u64::from_be_bytes(lsn_bytes);
        let location = DiskLocation(lsn);
        let (base_location, file_and_metadata) = files.range_mut(..=location).next_back().unwrap();

        let file_offset = lsn - base_location.0;
        let file = &mut file_and_metadata.file;

        file.seek(io::SeekFrom::Start(file_offset))?;

        let mut buf = BufReader::new(file);

        let mut header_buf = [0_u8; 20];
        buf.read_exact(&mut header_buf)?;

        let crc_expected_buf: [u8; 4] = header_buf[0..4].try_into().unwrap();
        let pid_buf: [u8; 8] = header_buf[4..12].try_into().unwrap();
        let len_buf: [u8; 8] = header_buf[12..].try_into().unwrap();

        let crc_expected = u32::from_le_bytes(crc_expected_buf);
        let pid = PageId(u64::from_le_bytes(pid_buf));
        let len: usize = if let Ok(len) = u64::from_le_bytes(len_buf).try_into() {
            len
        } else {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "corrupted length detected",
            ));
        };

        let mut page_buf = vec![0; len].into_boxed_slice();

        buf.read_exact(&mut page_buf)?;

        Ok(page_buf)
    }

    pub fn write_batch(&self, pages: Vec<(PageId, Vec<u8>)>) -> io::Result<()> {
        let shard = 0; // todo!();
        let lsn = self.next_file_lsn;
        let size_class = 0; // todo!();
        let gen = 0; // todo!();

        let mut new_locations: Vec<(PageId, DiskLocation)> = vec![];
        let mut buf = vec![];

        let mut capacity = 0;
        for (pid, raw_page) in pages {
            capacity += 1;
            let address = DiskLocation(lsn + buf.len() as u64);
            new_locations.push((pid, address));

            let len_buf: [u8; 8] = (raw_page.len() as u64).to_le_bytes();
            let pid_buf: [u8; 8] = pid.0.to_le_bytes();

            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&len_buf);
            hasher.update(&pid_buf);
            hasher.update(&raw_page);
            let crc: u32 = hasher.finalize();

            buf.write_all(&crc.to_le_bytes())?;
            buf.write_all(&pid_buf)?;
            buf.write_all(&len_buf)?;
            buf.write_all(&raw_page)?;
        }

        self.next_file_lsn += buf.len() as u64 + 1;

        let fname = format!(
            "{:02x}-{:016x}-{:01x}-{:01x}-{:016x}",
            shard, lsn, size_class, gen, capacity
        );

        let tmp_fname = format!("{}-tmp", fname);

        let new_path = self.config.path.join(HEAP_DIR_SUFFIX).join(fname);
        let tmp_path = self.config.path.join(HEAP_DIR_SUFFIX).join(tmp_fname);

        let mut tmp_options = OpenOptions::new();
        tmp_options.read(false).write(true).create(true);

        let mut tmp_file = tmp_options.open(&tmp_path)?;

        tmp_file.write_all(&buf)?;
        drop(buf);

        // mv and fsync new file and directory

        tmp_file.sync_all()?;
        drop(tmp_file);

        fs::rename(tmp_path, &new_path)?;

        let mut new_options = OpenOptions::new();
        new_options.read(true).write(false).create(false);

        let new_file = new_options.open(new_path)?;
        // TODO add new file to self.files with its metadata

        File::open(self.config.path.join(HEAP_DIR_SUFFIX)).and_then(|f| f.sync_all())?;

        // write a batch of updates to the pt

        let write_batch: Vec<([u8; 8], Option<[u8; 8]>)> = new_locations
            .into_iter()
            .map(|(pid, location)| {
                let key = pid.0.to_be_bytes();
                let value = Some(location.0.to_be_bytes());
                (key, value)
            })
            .chain(std::iter::once({
                // always mark the lsn w/ the pt batch
                let key = PT_LSN_KEY;
                let value = Some(lsn.to_le_bytes());
                (key, value)
            }))
            .collect();

        let mut pt = self.pt.write().unwrap();
        pt.write_batch(&write_batch)?;
        pt.flush()?;
        drop(pt);

        Ok(())
    }

    pub fn maintenance(&self) -> io::Result<()> {
        // TODO make this concurrency-friendly, because right now it blocks everything

        // scan files, filter by fragmentation, group by
        // generation and size class

        let mut files_to_defrag: Vec<&FileAndMetadata> = vec![];
        let mut locations_to_remove = vec![];
        let mut paths_to_remove = vec![];

        let files = self.files.read().unwrap();
        for (_, meta) in &*files {
            let len = meta.len.load(Ordering::Acquire);
            let cap = meta.capacity.max(1);

            if len == 0 {
                paths_to_remove.push(meta.path.clone());
            } else if (len * 100) / cap < u64::from(self.config.file_compaction_percent) {
                paths_to_remove.push(meta.path.clone());
                locations_to_remove.push(meta.location);
                files_to_defrag.push(meta);
            }
        }

        let pt = self.pt.read().unwrap();

        // rewrite the live pages
        let page_rewrite_iter = FilteredPageRewriteIter::new(&pt, &files_to_defrag);

        let batch = page_rewrite_iter.collect();

        drop(pt);
        drop(files);

        self.write_batch(batch)?;

        // get writer file lock and remove the replaced files

        let mut files = self.files.write().unwrap();

        for location in locations_to_remove {
            files.remove(&location);
        }

        drop(files);

        for path in paths_to_remove {
            std::fs::remove_file(path)?;
        }

        Ok(())
    }
}

struct FilteredPageRewriteIter<'a> {
    pt: &'a Lsm<8, 8>,
    files: Vec<&'a FileAndMetadata>,
}

impl<'a> FilteredPageRewriteIter<'a> {
    fn new(pt: &Lsm<8, 8>, files: &Vec<&FileAndMetadata>) -> FilteredPageRewriteIter<'a> {
        todo!()
    }
}

impl<'a> Iterator for FilteredPageRewriteIter<'a> {
    type Item = (PageId, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        todo!()
    }
}

#[test]
fn test_01() {
    fs::remove_dir_all("test_01");
    let mut m = Marble::open("test_01").unwrap();

    for i in 0..10 {
        let start = i * 10;
        let end = (i + 1) * 10;

        let mut batch = vec![];
        for pid in start..end {
            batch.push((PageId(pid), pid.to_be_bytes().to_vec()));
        }

        m.write_batch(batch).unwrap();
    }

    for pid in 0..100 {
        let read = m.read(PageId(pid)).unwrap();
        let expected = pid.to_be_bytes();
        assert_eq!(&*read, &expected[..]);
    }
}
