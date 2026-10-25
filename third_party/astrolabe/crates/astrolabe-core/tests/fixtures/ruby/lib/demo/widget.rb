require "foo/bar"
require_relative '../x'
autoload :Baz, "baz/q"
module A::B
  X, Y = 1, 2
  C::D = 3
  class Foo < Bar
    attr_accessor :a, :b
    alias c d
    alias_method :e, :f
    define_method(:g) { }
    class << self
      def h; end
    end
    def self.i; end
    def ==(o) end
    def j=(v); end
    private
    def k; end
    private def l; end
  end
end
def top; end
