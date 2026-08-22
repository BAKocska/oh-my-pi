//! Live-session `attachment://N` resolver.

use omp_core::{CowBytes, Hash32, Str};
use omp_storage::blob::{BlobRef, BlobStore};
use omp_tools::read::{
	Fault,
	resolver::{LineOffsetCache, Resolve},
	selector::ParsedSelector,
};

pub(super) struct AttachmentUrlResolver {
	store:   BlobStore,
	session: Str,
	lines:   LineOffsetCache,
}

impl AttachmentUrlResolver {
	pub(super) fn new(store: BlobStore, session: &str) -> Self {
		Self { store, session: Str::new(session), lines: LineOffsetCache::default() }
	}
}

impl Resolve for AttachmentUrlResolver {
	async fn read<'a>(
		&'a self,
		resource: &'a str,
		selector: &'a ParsedSelector,
	) -> Result<CowBytes<'static>, Fault> {
		let snapshot =
			omp_agent::attachments::session_attachments(&self.session).ok_or_else(|| {
				Fault::Source { message: Str::new_static("No live attachment snapshot is available.") }
			})?;
		let uri = format!("attachment://{resource}");
		let attachment = snapshot.resolve(&uri).map_err(|_| Fault::Source {
			message: Str::new_static("Attachment was not found in the latest user image set."),
		})?;
		let bytes = if !attachment.blob.inline.is_empty() {
			CowBytes::from(attachment.blob.inline.clone())
		} else {
			let hash = <[u8; 32]>::try_from(attachment.blob.hash.as_ref()).map_err(|_| {
				Fault::Source { message: Str::new_static("Attachment has an invalid content digest.") }
			})?;
			let reference = BlobRef { hash: Hash32::new(hash), size: attachment.blob.size };
			CowBytes::from(self.store.get(&reference).map_err(|_| Fault::Source {
				message: Str::new_static("Attachment content is unavailable."),
			})?)
		};
		super::select_bytes(&self.lines, resource, bytes, selector)
	}
}
