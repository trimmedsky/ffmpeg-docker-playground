require 'spec_helper'

describe 'pkg-config' do
  # Enabled by build option: filter, swresample, swscale (libpostproc was removed upstream in FFmpeg 8.0)
  %w(libavcodec libavfilter libavformat libavutil libswresample libswscale).each do |lib|
    describe lib do
      subject{ command "pkg-config #{lib} --modversion" }
      it('successfully found'){ expect(subject.exit_status).to eq 0 }
      it('no stderr output'){ expect(subject.stderr).to be_empty }
    end
  end
end

