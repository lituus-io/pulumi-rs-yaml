config count int {
	__logicalName = "count"
}

config isEnabled bool {
	__logicalName = "isEnabled"
	default = true
}

config name string {
	__logicalName = "name"
	default = "world"
}

resource bucket "aws:s3:Bucket" {
	__logicalName = "bucket"
	bucketPrefix = name
}
