config bucket string {
	__logicalName = "bucket"
}

resource bucketResource "aws:s3:Bucket" {
	__logicalName = "bucket"
}

output bucket0 {
	__logicalName = "bucket"
	value = bucket.id
}
