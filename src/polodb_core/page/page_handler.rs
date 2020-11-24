use std::fs::File;
use std::collections::BTreeMap;
use std::ops::Bound::{Included, Unbounded};
use std::rc::Rc;
use std::path::{Path, PathBuf};
use polodb_bson::Document;
use super::page::RawPage;
use super::pagecache::PageCache;
use super::header_page_wrapper;
use super::header_page_wrapper::HeaderPageWrapper;
use crate::journal::{JournalManager, TransactionType};
use crate::DbResult;
use crate::error::DbErr;
use crate::page::data_page_wrapper::DataPageWrapper;
use crate::data_ticket::DataTicket;

const DB_INIT_BLOCK_COUNT: u32 = 16;
const PRESERVE_WRAPPER_MIN_REMAIN_SIZE: u32 = 16;

#[derive(Eq, PartialEq)]
pub(crate) enum TransactionState {
    NoTrans,
    User,
    UserAuto,
    DbAuto,
}

pub(crate) struct PageHandler {
    file:                     File,

    pub last_commit_db_size:  u64,

    pub page_size:            u32,
    page_count:               u32,
    page_cache:               Box<PageCache>,
    journal_manager:          Box<JournalManager>,

    data_page_map:            BTreeMap<u32, Vec<u32>>,

    transaction_state:        TransactionState,

}

#[derive(Debug, Copy, Clone)]
pub(crate) struct AutoStartResult {
    pub auto_start: bool,
}

impl PageHandler {

    fn read_first_block(file: &mut File, page_size: u32) -> std::io::Result<RawPage> {
        let mut raw_page = RawPage::new(0, page_size);
        raw_page.read_from_file(file, 0)?;
        Ok(raw_page)
    }

    fn force_write_first_block(file: &mut File, page_size: u32) -> std::io::Result<RawPage> {
        let wrapper = HeaderPageWrapper::init(0, page_size);
        wrapper.0.sync_to_file(file, 0)?;
        Ok(wrapper.0)
    }

    fn init_db(file: &mut File, page_size: u32) -> std::io::Result<(RawPage, u32, u64)> {
        let meta = file.metadata()?;
        let file_len = meta.len();
        if file_len < page_size as u64 {
            let expected_file_size: u64 = (page_size as u64) * (DB_INIT_BLOCK_COUNT as u64);
            file.set_len(expected_file_size)?;
            let first_page = PageHandler::force_write_first_block(file, page_size)?;
            Ok((first_page, DB_INIT_BLOCK_COUNT as u32, expected_file_size))
        } else {
            let block_count = file_len / (page_size as u64);
            let first_page = PageHandler::read_first_block(file, page_size)?;
            Ok((first_page, block_count as u32, file_len))
        }
    }

    fn mk_journal_path(db_path: &Path) -> PathBuf {
        let mut buf = db_path.to_path_buf();
        let filename = buf.file_name().unwrap().to_str().unwrap();
        let new_filename = String::from(filename) + ".journal";
        buf.set_file_name(new_filename);
        buf
    }

    pub fn new(path: &Path, page_size: u32) -> DbResult<PageHandler> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(path)?;

        let (_, page_count, db_file_size) = PageHandler::init_db(&mut file, page_size)?;

        let journal_file_path: PathBuf = PageHandler::mk_journal_path(path);
        let journal_manager = JournalManager::open(&journal_file_path, page_size, db_file_size)?;

        let page_cache = PageCache::new_default(page_size);

        let last_commit_db_size = {
            let meta = file.metadata()?;
            meta.len()
        };

        Ok(PageHandler {
            file,

            last_commit_db_size,

            page_size,
            page_count,
            page_cache: Box::new(page_cache),
            journal_manager: Box::new(journal_manager),

            data_page_map: BTreeMap::new(),

            transaction_state: TransactionState::NoTrans,

        })
    }

    pub(crate) fn auto_start_transaction(&mut self, ty: TransactionType) -> DbResult<AutoStartResult> {
        let mut result = AutoStartResult { auto_start: false };
        match self.transaction_state {
            TransactionState::NoTrans => {
                self.start_transaction(ty)?;
                self.transaction_state = TransactionState::DbAuto;
                result.auto_start = true;
            }

            // current is auto-read, but going to write
            TransactionState::UserAuto => {
                match (ty, self.transaction_type()) {
                    (TransactionType::Write, Some(TransactionType::Read)) => {
                        self.upgrade_read_transaction_to_write()?;
                    }

                    _ => ()
                }
            }

            _ => ()
        }
        Ok(result)
    }

    pub(crate) fn auto_rollback(&mut self) -> DbResult<()> {
        if self.transaction_state == TransactionState::DbAuto {
            self.rollback()?;
            self.transaction_state = TransactionState::NoTrans;
        }
        Ok(())
    }

    pub(crate) fn auto_commit(&mut self) -> DbResult<()> {
        if self.transaction_state == TransactionState::DbAuto {
            self.commit()?;
            self.transaction_state = TransactionState::NoTrans;
        }
        Ok(())
    }

    pub(crate) fn distribute_data_page_wrapper(&mut self, data_size: u32) -> DbResult<DataPageWrapper> {
        let data_size = data_size + 2;  // preserve 2 bytes
        let (wrapper, removed_key) = {
            let mut range = self.data_page_map.range_mut((Included(data_size), Unbounded));
            match range.next() {
                Some((key, value)) => {
                    if value.is_empty() {
                        panic!("unexpected: distributed vector is empty");
                    }
                    let last_index = value[value.len() - 1];
                    value.remove(value.len() - 1);

                    let mut removed_key = None;

                    if value.is_empty() {
                        removed_key = Some(*key);
                    }

                    let raw_page = self.pipeline_read_page(last_index)?;
                    let wrapper = DataPageWrapper::from_raw(raw_page);

                    (wrapper, removed_key)
                },
                None => {
                    let wrapper = self.force_distribute_new_data_page_wrapper()?;
                    (wrapper, None)
                },
            }
        };

        removed_key.map(|key| {
            self.data_page_map.remove(&key);
        });

        Ok(wrapper)
    }

    #[inline]
    fn force_distribute_new_data_page_wrapper(&mut self) -> DbResult<DataPageWrapper> {
        let new_pid = self.alloc_page_id()?;
        let new_wrapper = DataPageWrapper::init(new_pid, self.page_size);
        Ok(new_wrapper)
    }

    pub(crate) fn return_data_page_wrapper(&mut self, wrapper: DataPageWrapper) {
        let remain_size = wrapper.remain_size();
        if remain_size < PRESERVE_WRAPPER_MIN_REMAIN_SIZE {
            return;
        }

        if wrapper.bar_len() >= (u16::max_value() as u32) / 2 {  // len too large
            return;
        }

        match self.data_page_map.get_mut(&remain_size) {
            Some(vector) => {
                vector.push(wrapper.pid());
            }

            None => {
                let vec = vec![ wrapper.pid() ];
                self.data_page_map.insert(remain_size, vec);
            }
        }
    }

    // 1. write to journal, if success
    //    - 2. checkpoint journal, if full
    // 3. write to page_cache
    pub fn pipeline_write_page(&mut self, page: &RawPage) -> Result<(), DbErr> {
        self.journal_manager.as_mut().append_raw_page(page)?;

        self.page_cache.insert_to_cache(page);
        Ok(())
    }

    // 1. read from page_cache, if none
    // 2. read from journal, if none
    // 3. read from main db
    pub fn pipeline_read_page(&mut self, page_id: u32) -> Result<RawPage, DbErr> {
        if let Some(page) = self.page_cache.get_from_cache(page_id) {
            #[cfg(feature = "log")]
            eprintln!("read page from cache, page_id: {}", page_id);

            return Ok(page);
        }

        if let Some(page) = self.journal_manager.read_page(page_id)? {
            // find in journal, insert to cache
            self.page_cache.insert_to_cache(&page);

            return Ok(page);
        }

        let offset = (page_id as u64) * (self.page_size as u64);
        let mut result = RawPage::new(page_id, self.page_size);
        result.read_from_file(&mut self.file, offset)?;

        self.page_cache.insert_to_cache(&result);

        #[cfg(feature = "log")]
        eprintln!("read page from main file, id: {}", page_id);

        Ok(result)
    }

    pub(crate) fn get_doc_from_ticket(&mut self, data_ticket: &DataTicket) -> DbResult<Option<Rc<Document>>> {
        let page = self.pipeline_read_page(data_ticket.pid)?;
        let wrapper = DataPageWrapper::from_raw(page);
        let bytes = wrapper.get(data_ticket.index as u32);
        if let Some(bytes) = bytes {
            let doc = Document::from_bytes(bytes)?;
            return Ok(Some(Rc::new(doc)));
        }
        return Ok(None);
    }

    pub(crate) fn store_doc(&mut self, doc: &Document) -> DbResult<DataTicket> {
        let bytes = doc.to_bytes()?;
        let mut wrapper = self.distribute_data_page_wrapper(bytes.len() as u32)?;
        let index = wrapper.bar_len() as u16;
        let pid = wrapper.pid();
        wrapper.put(&bytes);

        self.pipeline_write_page(wrapper.borrow_page())?;

        self.return_data_page_wrapper(wrapper);

        Ok(DataTicket {
            pid,
            index,
        })
    }

    pub(crate) fn free_data_ticket(&mut self, data_ticket: &DataTicket) -> DbResult<Vec<u8>> {
        #[cfg(feature = "log")]
        eprintln!("free data ticket: {}", data_ticket);

        let page = self.pipeline_read_page(data_ticket.pid)?;
        let mut wrapper = DataPageWrapper::from_raw(page);
        let bytes = wrapper.get(data_ticket.index as u32).unwrap().to_vec();
        wrapper.remove(data_ticket.index as u32);
        if wrapper.is_empty() {
            self.free_page(data_ticket.pid)?;
        }
        let page = wrapper.consume_page();
        self.pipeline_write_page(&page)?;
        Ok(bytes)
    }

    #[inline]
    pub fn free_page(&mut self, pid: u32) -> DbResult<()> {
        self.free_pages(&[pid])
    }

    pub fn free_pages(&mut self, pages: &[u32]) -> DbResult<()> {
        #[cfg(feature = "log")]
        for pid in pages {
            eprintln!("free page, id: {}", *pid);
        }

        let first_page = self.pipeline_read_page(0)?;
        let mut first_page_wrapper = HeaderPageWrapper::from_raw_page(first_page);
        let free_list_pid = first_page_wrapper.get_free_list_page_id();
        if free_list_pid != 0 {
            unimplemented!();
        }

        let current_size = first_page_wrapper.get_free_list_size();
        if (current_size as usize) + pages.len() >= header_page_wrapper::HEADER_FREE_LIST_MAX_SIZE {
            unimplemented!();
        }

        first_page_wrapper.set_free_list_size(current_size + (pages.len() as u32));
        let mut counter = 0;
        for pid in pages {
            first_page_wrapper.set_free_list_content(current_size + counter, *pid);
            counter += 1;
        }

        self.pipeline_write_page(&first_page_wrapper.0)?;

        self.page_count -= pages.len() as u32;

        Ok(())
    }

    pub fn is_journal_full(&self) -> bool {
        self.journal_manager.len() >= 1000
    }

    pub fn checkpoint_journal(&mut self) -> DbResult<()> {
        self.journal_manager.checkpoint_journal(&mut self.file)
    }

    fn try_get_free_page_id(&mut self) -> DbResult<Option<u32>> {
        let first_page = self.get_first_page()?;
        let mut first_page_wrapper = HeaderPageWrapper::from_raw_page(first_page);

        let free_list_size = first_page_wrapper.get_free_list_size();
        if free_list_size == 0 {
            return Ok(None);
        }

        let result = first_page_wrapper.get_free_list_content(free_list_size - 1);
        first_page_wrapper.set_free_list_size(free_list_size - 1);

        self.pipeline_write_page(&first_page_wrapper.0)?;

        Ok(Some(result))
    }

    #[inline]
    pub fn get_first_page(&mut self) -> Result<RawPage, DbErr> {
        self.pipeline_read_page(0)
    }

    pub fn alloc_page_id(&mut self) -> DbResult<u32> {
        let page_id = match self.try_get_free_page_id()? {
            Some(page_id) =>  {

                #[cfg(feature = "log")]
                eprintln!("get new page_id from free list: {}", page_id);

                Ok(page_id)
            }

            None =>  {
                self.actual_alloc_page_id()
            }
        }?;

        self.page_count += 1;
        Ok(page_id)
    }

    fn actual_alloc_page_id(&mut self) -> DbResult<u32> {
        let first_page = self.get_first_page()?;
        let mut first_page_wrapper = HeaderPageWrapper::from_raw_page(first_page);

        let null_page_bar = first_page_wrapper.get_null_page_bar();
        first_page_wrapper.set_null_page_bar(null_page_bar + 1);

        if (null_page_bar as u64) >= self.last_commit_db_size {  // truncate file
            let expected_size = self.last_commit_db_size + (DB_INIT_BLOCK_COUNT * self.page_size) as u64;

            self.last_commit_db_size = expected_size;
        }

        self.pipeline_write_page(&first_page_wrapper.0)?;

        #[cfg(feature = "log")]
        eprintln!("alloc new page_id : {}", null_page_bar);

        Ok(null_page_bar)
    }

    #[inline]
    pub fn journal_file_path(&self) -> &Path {
        self.journal_manager.path()
    }

    #[inline]
    pub fn start_transaction(&mut self, ty: TransactionType) -> DbResult<()> {
        self.journal_manager.start_transaction(ty)
    }

    #[inline]
    pub fn transaction_type(&mut self) -> Option<TransactionType> {
        self.journal_manager.transaction_type()
    }

    #[inline]
    fn upgrade_read_transaction_to_write(&mut self) -> DbResult<()> {
        self.journal_manager.upgrade_read_transaction_to_write()
    }

    #[inline]
    pub fn set_transaction_state(&mut self, state: TransactionState) {
        self.transaction_state = state;
    }

    pub fn commit(&mut self) -> DbResult<()> {
        self.journal_manager.commit()?;
        if self.is_journal_full() {
            self.checkpoint_journal()?;
            #[cfg(feature = "log")]
            eprintln!("checkpoint journal finished");
        }
        Ok(())
    }

    // after the rollback
    // all the cache are wrong
    // cleat it
    pub fn rollback(&mut self) -> DbResult<()> {
        self.journal_manager.rollback()?;
        self.page_cache = Box::new(PageCache::new_default(self.page_size));
        Ok(())
    }

}
