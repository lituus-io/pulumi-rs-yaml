resource bucket "aws:s3:Bucket" {
	__logicalName = "bucket"
	bucketPrefix = "my-bucket"

	options {
		dependsOn = [prov]
		protect = true
		provider = prov
		ignoreChanges = [
			bucketPrefix,
			tags,
		]
		version = "4.38.0"
	}
}

resource prov "pulumi:providers:aws" {
	__logicalName = "prov"
	region = "us-west-2"
}
