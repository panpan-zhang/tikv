// Copyright 2016 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::{self, Ordering};
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::u64;

use storage::CF_DEFAULT;
use storage::types;
use raftstore::store::keys;
use rocksdb::{DB, Options, SliceTransform, DBEntryType, TablePropertiesCollector,
              TablePropertiesCollectorFactory};
use util::codec;
use util::codec::number::{NumberEncoder, NumberDecoder};

pub use rocksdb::CFHandle;

use super::cfs_diff;

pub fn get_cf_handle<'a>(db: &'a DB, cf: &str) -> Result<&'a CFHandle, String> {
    db.cf_handle(cf)
        .ok_or_else(|| format!("cf {} not found.", cf))
}

pub fn open(path: &str, cfs: &[&str]) -> Result<DB, String> {
    let mut opts = Options::new();
    opts.create_if_missing(false);
    let mut cfs_opts = vec![];
    for _ in 0..cfs.len() {
        cfs_opts.push(Options::new());
    }
    open_opt(opts, path, cfs, cfs_opts)
}

pub fn open_opt(opts: Options,
                path: &str,
                cfs: &[&str],
                cfs_opts: Vec<Options>)
                -> Result<DB, String> {
    let cfs_ref_opts: Vec<&Options> = cfs_opts.iter().collect();
    DB::open_cf(opts, path, cfs, &cfs_ref_opts)
}

pub struct CFOptions<'a> {
    cf: &'a str,
    options: Options,
}

impl<'a> CFOptions<'a> {
    pub fn new(cf: &'a str, options: Options) -> CFOptions<'a> {
        CFOptions {
            cf: cf,
            options: options,
        }
    }
}

pub fn new_engine(path: &str, cfs: &[&str]) -> Result<DB, String> {
    let mut db_opts = Options::new();
    db_opts.enable_statistics();
    let mut cfs_opts = Vec::with_capacity(cfs.len());
    for cf in cfs {
        cfs_opts.push(CFOptions::new(*cf, Options::new()));
    }
    new_engine_opt(path, db_opts, cfs_opts)
}

fn check_and_open(path: &str, mut db_opt: Options, cfs_opts: Vec<CFOptions>) -> Result<DB, String> {
    // If db not exist, create it.
    if !db_exist(path) {
        db_opt.create_if_missing(true);

        let mut cfs = vec![];
        let mut cfs_opts_ref = vec![];
        if let Some(x) = cfs_opts.iter().find(|x| x.cf == CF_DEFAULT) {
            cfs.push(CF_DEFAULT);
            cfs_opts_ref.push(&x.options);
        }
        let mut db = try!(DB::open_cf(db_opt, path, cfs.as_slice(), cfs_opts_ref.as_slice()));
        for x in &cfs_opts {
            if x.cf == CF_DEFAULT {
                continue;
            }
            try!(db.create_cf(x.cf, &x.options));
        }

        return Ok(db);
    }

    db_opt.create_if_missing(false);

    // List all column families in current db.
    let cfs_list = try!(DB::list_column_families(&db_opt, path));
    let existed: Vec<&str> = cfs_list.iter().map(|v| v.as_str()).collect();
    let needed: Vec<&str> = cfs_opts.iter().map(|x| x.cf).collect();

    // If all column families are exist, just open db.
    if existed == needed {
        let mut cfs = vec![];
        let mut cfs_opts_ref = vec![];
        for x in &cfs_opts {
            cfs.push(x.cf);
            cfs_opts_ref.push(&x.options);
        }

        return DB::open_cf(db_opt, path, cfs.as_slice(), cfs_opts_ref.as_slice());
    }

    // Open db.
    let common_opt = Options::new();
    let mut cfs = vec![];
    let mut cfs_opts_ref = vec![];
    for cf in &existed {
        cfs.push(*cf);
        match cfs_opts.iter().find(|x| x.cf == *cf) {
            Some(x) => {
                cfs_opts_ref.push(&x.options);
            }
            None => {
                cfs_opts_ref.push(&common_opt);
            }
        }
    }
    let mut db = DB::open_cf(db_opt, path, cfs.as_slice(), cfs_opts_ref.as_slice()).unwrap();

    // Drop discarded column families.
    //    for cf in existed.iter().filter(|x| needed.iter().find(|y| y == x).is_none()) {
    for cf in cfs_diff(&existed, &needed) {
        // Never drop default column families.
        if cf != CF_DEFAULT {
            try!(db.drop_cf(cf));
        }
    }

    // Create needed column families not existed yet.
    for cf in cfs_diff(&needed, &existed) {
        try!(db.create_cf(cf, &cfs_opts.iter().find(|x| x.cf == cf).unwrap().options));
    }

    Ok(db)
}

pub fn new_engine_opt(path: &str, opts: Options, cfs_opts: Vec<CFOptions>) -> Result<DB, String> {
    check_and_open(path, opts, cfs_opts)
}

fn db_exist(path: &str) -> bool {
    let path = Path::new(path);
    if !path.exists() || !path.is_dir() {
        return false;
    }

    // If path is not an empty directory, we say db exists. If path is not an empty directory
    // but db has not been created, DB::list_column_families will failed and we can cleanup
    // the directory by this indication.
    fs::read_dir(&path).unwrap().next().is_some()
}

pub struct FixedSuffixSliceTransform {
    pub suffix_len: usize,
}

impl FixedSuffixSliceTransform {
    pub fn new(suffix_len: usize) -> FixedSuffixSliceTransform {
        FixedSuffixSliceTransform { suffix_len: suffix_len }
    }
}

impl SliceTransform for FixedSuffixSliceTransform {
    fn transform<'a>(&mut self, key: &'a [u8]) -> &'a [u8] {
        let mid = key.len() - self.suffix_len;
        let (left, _) = key.split_at(mid);
        left
    }

    fn in_domain(&mut self, key: &[u8]) -> bool {
        key.len() >= self.suffix_len
    }

    fn in_range(&mut self, _: &[u8]) -> bool {
        true
    }
}

pub struct FixedPrefixSliceTransform {
    pub prefix_len: usize,
}

impl FixedPrefixSliceTransform {
    pub fn new(prefix_len: usize) -> FixedPrefixSliceTransform {
        FixedPrefixSliceTransform { prefix_len: prefix_len }
    }
}

impl SliceTransform for FixedPrefixSliceTransform {
    fn transform<'a>(&mut self, key: &'a [u8]) -> &'a [u8] {
        &key[..self.prefix_len]
    }

    fn in_domain(&mut self, key: &[u8]) -> bool {
        key.len() >= self.prefix_len
    }

    fn in_range(&mut self, _: &[u8]) -> bool {
        true
    }
}

pub struct NoopSliceTransform;

impl SliceTransform for NoopSliceTransform {
    fn transform<'a>(&mut self, key: &'a [u8]) -> &'a [u8] {
        key
    }

    fn in_domain(&mut self, _: &[u8]) -> bool {
        true
    }

    fn in_range(&mut self, _: &[u8]) -> bool {
        true
    }
}

pub trait DecodeU64 {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error>;
}

impl DecodeU64 for HashMap<Vec<u8>, Vec<u8>> {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error> {
        let v = try!(self.get(k.as_bytes()).ok_or(codec::Error::KeyNotFound));
        Cursor::new(v).decode_u64()
    }
}

impl<'a> DecodeU64 for HashMap<&'a [u8], &'a [u8]> {
    fn decode_u64(&self, k: &str) -> Result<u64, codec::Error> {
        let v = try!(self.get(k.as_bytes().as_ref()).ok_or(codec::Error::KeyNotFound));
        Cursor::new(v).decode_u64()
    }
}

#[derive(Clone, Debug, Default)]
pub struct UserProperties {
    pub min_ts: u64,
    pub max_ts: u64,
    pub num_keys: u64,
    pub num_puts: u64,
    pub num_deletes: u64,
    pub num_corrupts: u64,
}

impl UserProperties {
    pub fn new() -> UserProperties {
        UserProperties {
            min_ts: u64::MAX,
            max_ts: u64::MIN,
            num_keys: 0,
            num_puts: 0,
            num_deletes: 0,
            num_corrupts: 0,
        }
    }

    pub fn num_versions(&self) -> u64 {
        self.num_puts + self.num_deletes
    }

    pub fn add(&mut self, other: &UserProperties) {
        self.min_ts = cmp::min(self.min_ts, other.min_ts);
        self.max_ts = cmp::max(self.max_ts, other.max_ts);
        self.num_keys += other.num_keys;
        self.num_puts += other.num_puts;
        self.num_deletes += other.num_deletes;
        self.num_corrupts += other.num_corrupts;
    }

    pub fn encode(&self) -> HashMap<Vec<u8>, Vec<u8>> {
        let items = [("tikv.min_ts", self.min_ts),
                     ("tikv.max_ts", self.max_ts),
                     ("tikv.num_keys", self.num_keys),
                     ("tikv.num_puts", self.num_puts),
                     ("tikv.num_deletes", self.num_deletes),
                     ("tikv.num_corrupts", self.num_corrupts)];
        items.iter()
            .map(|&(k, v)| {
                let mut buf = Vec::with_capacity(8);
                buf.encode_u64(v).unwrap();
                (k.as_bytes().to_owned(), buf)
            })
            .collect()
    }

    pub fn decode<T: DecodeU64>(props: &T) -> Result<UserProperties, codec::Error> {
        let mut res = UserProperties::new();
        res.min_ts = try!(props.decode_u64("tikv.min_ts"));
        res.max_ts = try!(props.decode_u64("tikv.max_ts"));
        res.num_keys = try!(props.decode_u64("tikv.num_keys"));
        res.num_puts = try!(props.decode_u64("tikv.num_puts"));
        res.num_deletes = try!(props.decode_u64("tikv.num_deletes"));
        res.num_corrupts = try!(props.decode_u64("tikv.num_corrupts"));
        Ok(res)
    }
}

pub struct UserPropertiesCollector {
    props: UserProperties,
    last_key: Vec<u8>,
}

impl UserPropertiesCollector {
    fn new() -> UserPropertiesCollector {
        UserPropertiesCollector {
            props: UserProperties::new(),
            last_key: Vec::new(),
        }
    }
}

impl TablePropertiesCollector for UserPropertiesCollector {
    fn name(&self) -> &str {
        "tikv.user-properties-collector"
    }

    fn add(&mut self, key: &[u8], _: &[u8], entry_type: DBEntryType, _: u64, _: u64) {
        if !keys::validate_data_key(key) {
            return;
        }
        let (k, ts) = match types::split_encoded_key_on_ts(key) {
            Ok((k, ts)) => (k, ts),
            Err(_) => {
                // Should we ignore this or panic?
                self.props.num_corrupts += 1;
                return;
            }
        };
        self.props.min_ts = cmp::min(self.props.min_ts, ts);
        self.props.max_ts = cmp::max(self.props.max_ts, ts);
        if k.cmp(&self.last_key) != Ordering::Equal {
            self.props.num_keys += 1;
            self.last_key.clear();
            self.last_key.extend_from_slice(k);
        }
        match entry_type {
            DBEntryType::Put => self.props.num_puts += 1,
            DBEntryType::Delete => self.props.num_deletes += 1,
            _ => {}
        }
    }

    fn finish(&mut self) -> HashMap<Vec<u8>, Vec<u8>> {
        self.props.encode()
    }
}

#[derive(Default)]
pub struct UserPropertiesCollectorFactory {}

impl UserPropertiesCollectorFactory {
    pub fn new() -> UserPropertiesCollectorFactory {
        UserPropertiesCollectorFactory {}
    }
}

impl TablePropertiesCollectorFactory for UserPropertiesCollectorFactory {
    fn name(&self) -> &str {
        "tikv.user-properties-collector-factory"
    }

    fn create_table_properties_collector(&mut self, _: u32) -> Box<TablePropertiesCollector> {
        Box::new(UserPropertiesCollector::new())
    }
}

#[cfg(test)]
mod tests {
    use rocksdb::{DB, Options, DBEntryType, TablePropertiesCollector};
    use tempdir::TempDir;
    use storage::{Key, CF_DEFAULT};
    use raftstore::store::keys;
    use super::{check_and_open, CFOptions, UserProperties, UserPropertiesCollector};

    #[test]
    fn test_check_and_open() {
        let path = TempDir::new("_util_rocksdb_test_check_column_families").expect("");
        let path_str = path.path().to_str().unwrap();

        // create db when db not exist
        let cfs_opts = vec![CFOptions::new(CF_DEFAULT, Options::new())];
        check_and_open(path_str, Options::new(), cfs_opts).unwrap();
        column_families_must_eq(path_str, &[CF_DEFAULT]);

        // add cf1.
        let cfs_opts = vec![CFOptions::new(CF_DEFAULT, Options::new()),
                            CFOptions::new("cf1", Options::new())];
        check_and_open(path_str, Options::new(), cfs_opts).unwrap();
        column_families_must_eq(path_str, &[CF_DEFAULT, "cf1"]);

        // drop cf1.
        let cfs_opts = vec![CFOptions::new(CF_DEFAULT, Options::new())];
        check_and_open(path_str, Options::new(), cfs_opts).unwrap();
        column_families_must_eq(path_str, &[CF_DEFAULT]);

        // never drop default cf
        let cfs_opts = vec![];
        check_and_open(path_str, Options::new(), cfs_opts).unwrap();
        column_families_must_eq(path_str, &[CF_DEFAULT]);
    }

    fn column_families_must_eq(path: &str, excepted: &[&str]) {
        let opts = Options::new();
        let cfs_list = DB::list_column_families(&opts, path).unwrap();

        let mut cfs_existed: Vec<&str> = cfs_list.iter().map(|v| v.as_str()).collect();
        let mut cfs_excepted: Vec<&str> = excepted.iter().map(|v| *v).collect();
        cfs_existed.sort();
        cfs_excepted.sort();
        assert_eq!(cfs_existed, cfs_excepted);
    }

    #[test]
    fn test_user_properties() {
        let cases = [("ab", 2, DBEntryType::Put),
                     ("ab", 0, DBEntryType::Delete),
                     ("cd", 4, DBEntryType::Delete),
                     ("cd", 3, DBEntryType::Put),
                     ("ef", 6, DBEntryType::Put),
                     ("gh", 7, DBEntryType::Delete)];
        let mut collector = UserPropertiesCollector::new();
        for &(key, ts, entry_type) in &cases {
            let k = Key::from_raw(key.as_bytes()).append_ts(ts);
            let data_key = keys::data_key(k.encoded());
            collector.add(&data_key, &[0], entry_type, 0, 0);
        }
        let props = UserProperties::decode(&collector.finish()).unwrap();
        assert_eq!(props.min_ts, 0);
        assert_eq!(props.max_ts, 7);
        assert_eq!(props.num_keys, 4);
        assert_eq!(props.num_puts, 3);
        assert_eq!(props.num_deletes, 3);
    }
}
