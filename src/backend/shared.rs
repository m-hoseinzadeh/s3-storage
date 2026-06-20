//! A shareable, `Clone`-able handle to a single [`FileSystem`] that also
//! implements [`s3s::S3`].
//!
//! The admin panel and the public S3 service must operate on the *same*
//! [`FileSystem`] instance so they share its atomic temp-file counter (two
//! independent instances could pick colliding `.tmp.N.internal.part` names under
//! concurrent writes). [`FileSystem`] is not `Clone`, so we wrap it in an `Arc`
//! and forward the `S3` trait through to it. Method calls resolve to
//! `FileSystem`'s own `S3` implementation via `Arc` deref.

use std::sync::Arc;

use async_trait::async_trait;
use s3s::dto::*;
use s3s::{S3, S3Request, S3Response, S3Result};

use super::FileSystem;

/// Shared handle to a [`FileSystem`]. Cheap to clone.
#[derive(Debug, Clone)]
pub struct SharedFileSystem(pub Arc<FileSystem>);

impl SharedFileSystem {
    #[must_use]
    pub fn new(fs: Arc<FileSystem>) -> Self {
        Self(fs)
    }
}

#[async_trait]
impl S3 for SharedFileSystem {
    async fn create_bucket(&self, req: S3Request<CreateBucketInput>) -> S3Result<S3Response<CreateBucketOutput>> {
        self.0.create_bucket(req).await
    }

    async fn copy_object(&self, req: S3Request<CopyObjectInput>) -> S3Result<S3Response<CopyObjectOutput>> {
        self.0.copy_object(req).await
    }

    async fn delete_bucket(&self, req: S3Request<DeleteBucketInput>) -> S3Result<S3Response<DeleteBucketOutput>> {
        self.0.delete_bucket(req).await
    }

    async fn delete_object(&self, req: S3Request<DeleteObjectInput>) -> S3Result<S3Response<DeleteObjectOutput>> {
        self.0.delete_object(req).await
    }

    async fn delete_objects(&self, req: S3Request<DeleteObjectsInput>) -> S3Result<S3Response<DeleteObjectsOutput>> {
        self.0.delete_objects(req).await
    }

    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        self.0.get_bucket_location(req).await
    }

    async fn get_object(&self, req: S3Request<GetObjectInput>) -> S3Result<S3Response<GetObjectOutput>> {
        self.0.get_object(req).await
    }

    async fn head_bucket(&self, req: S3Request<HeadBucketInput>) -> S3Result<S3Response<HeadBucketOutput>> {
        self.0.head_bucket(req).await
    }

    async fn head_object(&self, req: S3Request<HeadObjectInput>) -> S3Result<S3Response<HeadObjectOutput>> {
        self.0.head_object(req).await
    }

    async fn list_buckets(&self, req: S3Request<ListBucketsInput>) -> S3Result<S3Response<ListBucketsOutput>> {
        self.0.list_buckets(req).await
    }

    async fn list_objects(&self, req: S3Request<ListObjectsInput>) -> S3Result<S3Response<ListObjectsOutput>> {
        self.0.list_objects(req).await
    }

    async fn list_objects_v2(&self, req: S3Request<ListObjectsV2Input>) -> S3Result<S3Response<ListObjectsV2Output>> {
        self.0.list_objects_v2(req).await
    }

    async fn put_object(&self, req: S3Request<PutObjectInput>) -> S3Result<S3Response<PutObjectOutput>> {
        self.0.put_object(req).await
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        self.0.create_multipart_upload(req).await
    }

    async fn upload_part(&self, req: S3Request<UploadPartInput>) -> S3Result<S3Response<UploadPartOutput>> {
        self.0.upload_part(req).await
    }

    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        self.0.upload_part_copy(req).await
    }

    async fn list_parts(&self, req: S3Request<ListPartsInput>) -> S3Result<S3Response<ListPartsOutput>> {
        self.0.list_parts(req).await
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        self.0.complete_multipart_upload(req).await
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        self.0.abort_multipart_upload(req).await
    }
}
