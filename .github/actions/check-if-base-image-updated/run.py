import sys, os, subprocess

def get_timestamp_str(image):
    subprocess.run(f'docker pull {image}'.split(' '), check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    r = subprocess.run(f"docker image inspect {image}".split(' ') + ['--format', r'{{.Created}}'], check=True, capture_output=True)
    r = r.stdout.decode('utf-8').split('.')[0]
    r = ''.join(r.split(':'))
    r = ''.join(r.split('-'))
    r = '_'.join(r.split('T'))
    return r

if __name__ == '__main__':
    images = []
    with open(sys.argv[1], 'r') as f:
        for line in f.readlines():
            if line.startswith('FROM '):
                line = [s for s in line.split() if s]
                images += [line[1]]

    print('Checking image', os.environ['package_tag'])
    ref_timestamp = '00000000_000000'
    try:
        ref_timestamp = get_timestamp_str(os.environ['package_tag'])
    except:
        pass
    print('Latest package T=', ref_timestamp, sep='')

    ret = 0
    for image in images:
        print('Checking image', image, '... ', end='')
        r = get_timestamp_str(image)
        if r > ref_timestamp:
            print('has an update, T=', r, sep='')
            ret = 1
        else:
            print('no update, T=', r, sep='')

    print(f'::set-output name=result::{ret}')