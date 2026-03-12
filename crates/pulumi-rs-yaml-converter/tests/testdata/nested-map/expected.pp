resource myBucket "aws:s3:Bucket" {
	__logicalName = "my-bucket"
	website = {
		indexDocument = "index.html"
	}
}
