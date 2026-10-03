short = "eylsvqy8"

suffix = "eyls"

resource bucket "gcp:storage:Bucket" {
	__logicalName = "bucket"
}

resource svc "gcp:cloudrunv2:Service" {
	__logicalName = "svc"
	name = "collector-${suffix}"
	description = null /* unsupported builtin */
}

output shortSuffix {
	__logicalName = "shortSuffix"
	value = short
}

output svcName {
	__logicalName = "svcName"
	value = svc.name
}
