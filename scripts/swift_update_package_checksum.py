import argparse
import json
import os
import re
import sys


def run(new_checksum: str = None, new_tag: str = None):
	if new_checksum is None and new_tag is None:
		print('At least one of --checksum or --tag arguments must be provided.', file=sys.stderr)
		sys.exit(1)

	if new_checksum is not None:
		if not new_checksum.isalnum():
			print('Checksum must be alphanumeric.', file=sys.stderr)
			sys.exit(1)

		if not new_checksum.islower():
			print('Checksum must be lowercase.', file=sys.stderr)
			sys.exit(1)

		try:
			int(new_checksum, 16)
		except:
			print('Checksum must be hexadecimal.', file=sys.stderr)
			sys.exit(1)

	if new_tag is not None:
		if new_tag.strip() != new_tag:
			print('Tag must not contain any whitespace.', file=sys.stderr)

		tag_regex = re.compile("^\d+[.]\d+[.]\d+$")
		tag_match = tag_regex.match(new_tag)
		if tag_match is None:
			print('Tag must adhere to x.x.x major/minor/patch format.', file=sys.stderr)

	settings = [
		{'variable_name': 'checksum', 'value': new_checksum},
		{'variable_name': 'tag', 'value': new_tag},
	]

	package_file_path = os.path.realpath(os.path.join(os.path.dirname(__file__), '../Package.swift'))
	print(package_file_path)



	original_package_file = None
	try:
		with open(package_file_path, 'r') as package_file_handle:
			original_package_file = package_file_handle.read()
	except:
		print('Failed to read Package.swift file.', file=sys.stderr)
		sys.exit(1)

	package_file = original_package_file
	for current_setting in settings:
		current_variable_name = current_setting['variable_name']
		new_value = current_setting['value']
		if new_value is None:
			continue

		print(f'setting {current_variable_name} (JSON-serialization):')
		print(json.dumps(new_value))

		regex = re.compile(f'(let[\s]+{current_variable_name}[\s]*=[\s]*)(.*)')

		previous_value = regex.search(package_file).group(2)
		package_file = package_file.replace(previous_value, f'"{new_value}"')

	with open(package_file_path, "w") as f:
		f.write(package_file)



if __name__ == '__main__':
	parser = argparse.ArgumentParser(description='Process some integers.')
	parser.add_argument('--checksum', type=str, help='new checksum of LDKNode.xcframework.zip', required=False, default=None)
	parser.add_argument('--tag', type=str, help='new release tag', required=False, default=None)
	args = parser.parse_args()
	run(new_checksum=args.checksum, new_tag=args.tag)
