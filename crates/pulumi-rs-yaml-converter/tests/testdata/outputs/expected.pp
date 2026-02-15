resource bucket "aws:s3:Bucket" {
	__logicalName = "bucket"
}

output bucketArn {
	__logicalName = "bucketArn"
	value = bucket.arn
}

output bucketName {
	__logicalName = "bucketName"
	value = bucket.id
}
