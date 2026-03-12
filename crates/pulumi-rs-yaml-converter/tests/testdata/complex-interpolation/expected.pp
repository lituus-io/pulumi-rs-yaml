resource bucket "aws:s3:Bucket" {
	__logicalName = "bucket"
}

output url {
	__logicalName = "url"
	value = "https://${bucket.bucketDomainName}/index.html"
}
