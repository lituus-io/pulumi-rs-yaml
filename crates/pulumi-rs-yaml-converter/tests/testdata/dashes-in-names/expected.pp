resource myBucket "aws:s3:Bucket" {
	__logicalName = "my-bucket"
	bucketPrefix = "test"
}

output bucketName {
	__logicalName = "bucket-name"
	value = myBucket.id
}
