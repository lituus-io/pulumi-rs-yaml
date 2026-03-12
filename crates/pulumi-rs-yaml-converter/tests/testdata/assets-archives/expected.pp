resource site "aws:s3:BucketObject" {
	__logicalName = "site"
	content = stringAsset("<h1>Hello</h1>")
	source = fileAsset("./index.html")
	archive = fileArchive("./site")
}
