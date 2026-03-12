resource bucket "aws:s3:Bucket" {
	__logicalName = "bucket"
	bucketName = "my-bucket"
}

component myApp "./myApp" {
	__logicalName = "myApp"
	env = "prod" // type: string
}
