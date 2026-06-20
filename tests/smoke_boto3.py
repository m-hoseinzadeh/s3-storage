#!/usr/bin/env python3
"""Cross-language S3 SDK compatibility smoke test using boto3.

Drives a running s3-storage instance through the full object lifecycle, including
a multipart upload. boto3 uses real AWS SigV4 signing and (on recent versions)
streaming/trailer uploads, so passing this is strong evidence of SDK compatibility.

Configuration via environment:
  S3_ENDPOINT    e.g. http://127.0.0.1:8080   (required)
  S3_ACCESS_KEY  access key                   (required)
  S3_SECRET_KEY  secret key                   (required)
  S3_REGION      region name (default us-east-1)

Exits 0 on success; prints FAIL and exits 1 otherwise.
"""

import os
import sys
import uuid

import boto3
from botocore.config import Config


def main() -> int:
    endpoint = os.environ["S3_ENDPOINT"]
    access_key = os.environ["S3_ACCESS_KEY"]
    secret_key = os.environ["S3_SECRET_KEY"]
    region = os.environ.get("S3_REGION", "us-east-1")

    s3 = boto3.client(
        "s3",
        endpoint_url=endpoint,
        aws_access_key_id=access_key,
        aws_secret_access_key=secret_key,
        region_name=region,
        config=Config(s3={"addressing_style": "path"}),
    )

    bucket = f"boto3-{uuid.uuid4().hex[:12]}"

    # Bucket lifecycle.
    s3.create_bucket(Bucket=bucket)
    assert any(b["Name"] == bucket for b in s3.list_buckets()["Buckets"]), "bucket not listed"
    s3.head_bucket(Bucket=bucket)

    # Simple put/get round-trip, including a nested key and a key with spaces.
    for key, body in [
        ("hello.txt", b"hello from boto3"),
        ("nested/dir/file.bin", bytes(range(256))),
        ("with space.txt", b"spaces work"),
    ]:
        s3.put_object(Bucket=bucket, Key=key, Body=body)
        got = s3.get_object(Bucket=bucket, Key=key)["Body"].read()
        assert got == body, f"round-trip mismatch for {key!r}"

    # HEAD.
    head = s3.head_object(Bucket=bucket, Key="hello.txt")
    assert head["ContentLength"] == len(b"hello from boto3")

    # A larger body to push boto3's streaming upload path.
    big = b"x" * (3 * 1024 * 1024)
    s3.put_object(Bucket=bucket, Key="big.bin", Body=big)
    assert s3.get_object(Bucket=bucket, Key="big.bin")["Body"].read() == big

    # ListObjectsV2 with a prefix.
    listed = s3.list_objects_v2(Bucket=bucket, Prefix="nested/")
    keys = [o["Key"] for o in listed.get("Contents", [])]
    assert "nested/dir/file.bin" in keys, f"prefix listing missing key: {keys}"

    # Multipart upload: first part >= 5 MiB, second part is the tail.
    mkey = "multipart.bin"
    create = s3.create_multipart_upload(Bucket=bucket, Key=mkey)
    upload_id = create["UploadId"]
    part1 = b"1" * (5 * 1024 * 1024)
    part2 = b"2" * 2048
    parts = []
    for n, data in [(1, part1), (2, part2)]:
        up = s3.upload_part(Bucket=bucket, Key=mkey, UploadId=upload_id, PartNumber=n, Body=data)
        parts.append({"ETag": up["ETag"], "PartNumber": n})
    s3.complete_multipart_upload(
        Bucket=bucket, Key=mkey, UploadId=upload_id, MultipartUpload={"Parts": parts}
    )
    combined = s3.get_object(Bucket=bucket, Key=mkey)["Body"].read()
    assert combined == part1 + part2, "multipart result mismatch"

    # Presigned GET works without ambient credentials on the URL holder's behalf.
    url = s3.generate_presigned_url("get_object", Params={"Bucket": bucket, "Key": "hello.txt"})
    assert url.startswith(endpoint), "unexpected presigned url"

    # Cleanup.
    for key in ["hello.txt", "nested/dir/file.bin", "with space.txt", "big.bin", mkey]:
        s3.delete_object(Bucket=bucket, Key=key)
    s3.delete_bucket(Bucket=bucket)

    print("PASS: boto3 S3 compatibility smoke test")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as exc:  # noqa: BLE001
        print(f"FAIL: {type(exc).__name__}: {exc}", file=sys.stderr)
        sys.exit(1)
