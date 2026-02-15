resource myBucket "aws:s3:Bucket" {
	__logicalName = "myBucket"
	bucketPrefix = "my-bucket"
}
