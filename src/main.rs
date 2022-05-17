#![feature(try_trait_v2)]

use anyhow::Result;
use parking_lot::Mutex;
use rand::Rng;
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::{
  collections::{BTreeMap, HashMap},
  env,
  fs::{remove_dir_all, remove_file},
  mem::MaybeUninit,
  time::Instant,
};

#[allow(non_upper_case_globals)]
pub mod index {
  pub const test: &str = "test";
}

pub fn run<const N: usize>() -> Result<()> {
  let dir = env::temp_dir();
  let mut rng = rand::thread_rng();
  let mut li: [[u64; 2]; N] = unsafe { MaybeUninit::uninit().assume_init() };
  for i in 0..N {
    li[i][0] = rng.gen();
    li[i][1] = rng.gen();
  }

  macro_rules! elapsed {
    ($op:ident, $func:expr, $iter:ident) => {{
      let now = Instant::now();
      li.$iter().try_for_each($func)?;
      let elapsed = now.elapsed();
      println!(
        "* {} {:.3} 万次/秒",
        stringify!($op),
        N as f64 * 100000.0 / elapsed.as_nanos() as f64
      );
      Ok::<_, anyhow::Error>(())
    }?};
    ($op:ident, $func:expr) => {
      elapsed!($op, $func, into_par_iter)
    };
  }
  let n = AtomicU64::new(0);

  macro_rules! n_add {
    ($i:expr) => {{
      let i: &[u8] = &$i;
      n.fetch_xor(u64::from_le_bytes(i.try_into().unwrap()), Ordering::SeqCst);
    }};
  }

  {
    use yakv::storage::{Select, Storage, StorageConfig};

    let filename = "yakv";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_dir_all(&dbpath);
    let db = Storage::open(
      &dbpath,
      StorageConfig {
        cache_size: 128 * 1024, // 1Gb
        nosync: true,
      },
    )?;

    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      db.put(k.to_be_bytes().to_vec(), v.to_le_bytes().to_vec())?;
      Ok(())
    });

    elapsed!(get, |kv| -> Result<()> {
      let [k, _] = kv;
      if let Some(i) = db.get(&k.to_be_bytes().to_vec())? {
        n_add!(i)
      }
      Ok(())
    });
  }

  {
    use rusty_leveldb::{Options, DB};

    let filename = "rusty_leveldb";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_dir_all(&dbpath);

    let mut db = DB::open(dbpath, Options::default())?;

    elapsed!(
      insert,
      |kv| -> Result<()> {
        let [k, v] = kv;
        db.put(&k.to_be_bytes(), &v.to_le_bytes())?;
        Ok(())
      },
      iter
    );
    elapsed!(
      get,
      |kv| -> Result<()> {
        let [k, _] = kv;
        if let Some(i) = db.get(&k.to_be_bytes()) {
          n_add!(i)
        }
        Ok(())
      },
      iter
    );
  }

  {
    use duckdb::{params, DuckdbConnectionManager};

    let filename = "duckdb";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_file(&dbpath);
    let manager = DuckdbConnectionManager::file(dbpath)?;
    let pool = r2d2::Pool::new(manager)?;

    pool.get()?.execute(
      "CREATE TABLE IF NOT EXISTS test (id BIGINT PRIMARY KEY,val BIGINT)",
      params![],
    )?;
    /*let mut stmt = pool
    .get()?
    .prepare("INSERT INTO test (id,val) VALUES (?,?)")?;*/
    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      pool
        .get()?
        .execute("INSERT INTO test (id,val) VALUES (?,?)", [k, v])?;
      Ok(())
    });
    elapsed!(get, |kv| -> Result<()> {
      pool
        .get()?
        .execute("SELECT val FROM test WHERE id=?", [kv[0]])?;
      Ok(())
    });
  }

  {
    let filename = "rocksdb";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_dir_all(&dbpath);
    let mut opt = rocksdb::Options::default();
    opt.create_if_missing(true);
    opt.set_max_open_files(10000);
    opt.set_use_fsync(false);
    opt.set_bytes_per_sync(8388608);
    opt.optimize_for_point_lookup(1024);
    opt.set_table_cache_num_shard_bits(6);
    opt.set_max_write_buffer_number(32);
    opt.set_write_buffer_size(536870912);
    opt.set_target_file_size_base(1073741824);
    opt.set_min_write_buffer_number_to_merge(4);
    opt.set_level_zero_stop_writes_trigger(2000);
    opt.set_level_zero_slowdown_writes_trigger(0);
    opt.set_compaction_style(rocksdb::DBCompactionStyle::Universal);
    opt.set_max_background_jobs(4);
    opt.set_disable_auto_compactions(true);
    let db = Arc::new(rocksdb::DB::open(&opt, dbpath)?);

    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      db.put(&k.to_be_bytes(), &v.to_le_bytes())?;
      Ok(())
    });

    elapsed!(get, |kv| -> Result<()> {
      let [k, _] = kv;
      if let Some(i) = db.get_pinned(&k.to_be_bytes())? {
        n_add!(i)
      }
      Ok(())
    });
  }
  {
    use persy::{Config, Persy, TransactionConfig, ValueMode};
    let filename = "persy";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_file(&dbpath);

    let tx_config = TransactionConfig::new().set_background_sync(true);

    let db = Persy::open_or_create_with(dbpath, Config::new(), |db| {
      let mut tx = db.begin()?;
      //tx.create_segment("test")?;
      tx.create_index::<u64, u64>(index::test, ValueMode::Replace)?;
      tx.prepare()?.commit()?;
      Ok(())
    })?;
    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      let mut tx = db.begin_with(tx_config.clone())?;
      tx.put(index::test, k, v)?;
      tx.prepare()?.commit()?;
      Ok(())
    });

    elapsed!(get, |kv| -> Result<()> {
      let [k, _] = kv;
      for (_, vli) in db.range::<u64, u64, _>(index::test, k..k + 1)? {
        for i in vli {
          n_add!(i.to_le_bytes());
          break;
        }
        break;
      }
      Ok(())
    });
  }

  {
    let filename = "sled";
    println!("\n# {filename}");
    let dbpath = dir.join(filename);
    let _ = remove_dir_all(&dbpath);

    let db = sled::open(dbpath)?;
    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      db.insert(&k.to_be_bytes(), &v.to_le_bytes())?;
      Ok(())
    });
    elapsed!(get, |kv| -> Result<()> {
      let [k, _] = kv;
      if let Some(i) = db.get(&k.to_be_bytes())? {
        n_add!(i)
      }
      Ok(())
    });
  }

  macro_rules! map {
    ($name:ty) => {
      println!("\n# {}", stringify!($name));
      let db = Arc::new(Mutex::new(<$name>::new()));

      elapsed!(insert, |kv| -> Result<()> {
        let [k, v] = kv;
        db.lock().insert(k, v);
        Ok(())
      });

      elapsed!(get, |kv| -> Result<()> {
        let [k, _] = kv;
        if let Some(i) = db.lock().get(&k) {
          n_add!(i.to_le_bytes())
        }
        Ok(())
      });
    };
  }

  map!(BTreeMap::<u64, u64>);
  map!(HashMap::<u64, u64>);
  map!(btree_slab::BTreeMap::<u64, u64>);
  {
    use dashmap::DashMap;
    println!("\n# dashmap");
    let db = DashMap::new();

    elapsed!(insert, |kv| -> Result<()> {
      let [k, v] = kv;
      db.insert(k, v);
      Ok(())
    });

    elapsed!(get, |kv| -> Result<()> {
      let [k, _] = kv;
      if let Some(i) = db.get(&k) {
        n_add!(i.to_le_bytes());
      }
      Ok(())
    });
  }
  Ok(())
}

fn main() -> Result<()> {
  run::<10000>()
}
